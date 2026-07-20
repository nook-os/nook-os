// Unlocking the vault with a passkey.
//
// A passkey does not replace the app password — it carries it. WebAuthn's PRF
// extension turns an authenticator into a key: ask it to evaluate a fixed
// salt and it returns the same 32 bytes every time, but only after the user
// proves themselves (touch, face, PIN) and only on this origin. We use those
// bytes to encrypt the app password and hand the ciphertext to the server.
//
// So the server gains no new power: it stores a blob it can't open, exactly
// as it stores secrets it can't read. And losing every passkey costs nothing
// but convenience — typing the app password still works, which is why this is
// a shortcut rather than a second source of truth.
//
// PRF needs a secure context (https, or localhost) and an authenticator that
// supports it. Everything here degrades to "not supported", never to an
// error dialog.
import { api } from "@nookos/api";

/** Fixed PRF input: same salt every time so the derived key is stable. */
const PRF_SALT = new TextEncoder().encode("nookos:vault:v1");

const b64 = {
  encode: (bytes: ArrayBuffer | Uint8Array): string => {
    const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    let s = "";
    for (const b of view) s += String.fromCharCode(b);
    return btoa(s);
  },
  decode: (text: string): Uint8Array => {
    const bin = atob(text);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  },
  // WebAuthn credential ids travel as base64url.
  encodeUrl: (bytes: ArrayBuffer): string =>
    b64.encode(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, ""),
  decodeUrl: (text: string): Uint8Array =>
    b64.decode(text.replace(/-/g, "+").replace(/_/g, "/")),
};

export function passkeysSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    !!window.PublicKeyCredential &&
    !!navigator.credentials &&
    window.isSecureContext
  );
}

/** Turn PRF output into an AES-GCM key. */
async function keyFromPrf(prf: ArrayBuffer): Promise<CryptoKey> {
  const material = await crypto.subtle.importKey("raw", prf, "HKDF", false, [
    "deriveKey",
  ]);
  return crypto.subtle.deriveKey(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: new Uint8Array(0),
      info: new TextEncoder().encode("nookos-vault-wrap"),
    },
    material,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

type PrfResults = { first?: ArrayBuffer };

function prfOutput(cred: PublicKeyCredential): ArrayBuffer | null {
  const ext = cred.getClientExtensionResults() as {
    prf?: { results?: PrfResults };
  };
  return ext.prf?.results?.first ?? null;
}

/**
 * Create a passkey and wrap the app password with it.
 *
 * Two ceremonies, not one: registration doesn't reliably return PRF output
 * across browsers, so we create the credential and then immediately assert it
 * to get the key material. The user sees two prompts once, and never again.
 */
export async function enrollPasskey(
  passphrase: string,
  userName: string,
  label: string,
): Promise<boolean> {
  if (!passkeysSupported()) return false;

  const userId = crypto.getRandomValues(new Uint8Array(16));
  const created = (await navigator.credentials.create({
    publicKey: {
      challenge: crypto.getRandomValues(new Uint8Array(32)),
      rp: { name: "NookOS", id: window.location.hostname },
      user: { id: userId, name: userName, displayName: userName },
      pubKeyCredParams: [
        { type: "public-key", alg: -7 }, // ES256
        { type: "public-key", alg: -257 }, // RS256
      ],
      authenticatorSelection: {
        residentKey: "required",
        userVerification: "required",
      },
      timeout: 60_000,
      extensions: { prf: { eval: { first: PRF_SALT } } },
    } as PublicKeyCredentialCreationOptions,
  })) as PublicKeyCredential | null;
  if (!created) return false;

  const credentialId = b64.encodeUrl(created.rawId);
  const prf = await assertPrf(credentialId);
  if (!prf) {
    // The authenticator made a credential but won't do PRF: it can't carry
    // the password, so enrolling it would promise an unlock we can't deliver.
    throw new Error("this passkey can't store an encryption key (no PRF support)");
  }

  const key = await keyFromPrf(prf);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ciphertext = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv },
    key,
    new TextEncoder().encode(passphrase),
  );
  // iv || ciphertext, matching how the Rust side stores its own sealed blobs.
  const wrapped = new Uint8Array(iv.length + ciphertext.byteLength);
  wrapped.set(iv, 0);
  wrapped.set(new Uint8Array(ciphertext), iv.length);

  const { error } = await api.POST("/api/v1/vault/passkeys", {
    body: {
      credential_id: credentialId,
      label,
      wrapped_secret: b64.encode(wrapped),
    },
  });
  return !error;
}

/** Ask the authenticator to evaluate the PRF salt. */
async function assertPrf(credentialId?: string): Promise<ArrayBuffer | null> {
  const assertion = (await navigator.credentials.get({
    publicKey: {
      challenge: crypto.getRandomValues(new Uint8Array(32)),
      rpId: window.location.hostname,
      allowCredentials: credentialId
        ? [{ type: "public-key", id: b64.decodeUrl(credentialId) }]
        : [],
      userVerification: "required",
      timeout: 60_000,
      extensions: { prf: { eval: { first: PRF_SALT } } },
    } as PublicKeyCredentialRequestOptions,
  })) as PublicKeyCredential | null;
  return assertion ? prfOutput(assertion) : null;
}

/**
 * Unlock with a passkey. Returns the app password, or null if there's no
 * passkey to use or the user dismissed the prompt — in which case the caller
 * falls back to asking for the password.
 */
export async function unlockWithPasskey(): Promise<string | null> {
  if (!passkeysSupported()) return null;

  const { data: passkeys } = await api.GET("/api/v1/vault/passkeys", {});
  if (!passkeys?.length) return null;

  try {
    // Offer every enrolled passkey and let the platform pick — the user might
    // be on any of their machines.
    const assertion = (await navigator.credentials.get({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rpId: window.location.hostname,
        allowCredentials: passkeys.map((p) => ({
          type: "public-key" as const,
          id: b64.decodeUrl(p.credential_id),
        })),
        userVerification: "required",
        timeout: 60_000,
        extensions: { prf: { eval: { first: PRF_SALT } } },
      } as PublicKeyCredentialRequestOptions,
    })) as PublicKeyCredential | null;
    if (!assertion) return null;

    const prf = prfOutput(assertion);
    if (!prf) return null;

    const used = passkeys.find((p) => p.credential_id === b64.encodeUrl(assertion.rawId));
    if (!used) return null;

    const wrapped = b64.decode(used.wrapped_secret);
    const key = await keyFromPrf(prf);
    const plain = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: wrapped.slice(0, 12) },
      key,
      wrapped.slice(12),
    );

    void api.POST("/api/v1/vault/passkeys/{id}/used", {
      params: { path: { id: used.id } },
    });
    return new TextDecoder().decode(plain);
  } catch {
    // Cancelled, timed out, wrong device — all mean "ask for the password".
    return null;
  }
}
