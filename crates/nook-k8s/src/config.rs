//! Where the credentials come from, and in which order (MAIN-339 AC-1).
//!
//! IN-CLUSTER FIRST, kubeconfig second. `kube::Config::infer` resolves the
//! other way round, and for an executor that ordering is wrong: an agent Pod
//! whose image happens to carry a `~/.kube/config` — a base image with a
//! developer's dotfiles, a mounted home — would talk to whatever cluster that
//! file names instead of the one it is running in. The ServiceAccount it was
//! given is the answer whenever it exists.
//!
//! Everything the decision reads is a field of [`Environment`], so a test hands
//! it a fixture directory rather than needing a cluster. That is not a test
//! affordance bolted on: the same [`Environment::from_process`] → [`resolve`] →
//! [`Source::load`] path runs in a Pod, so what the tests exercise is the code
//! that ships. A resolver that could only be observed inside a real cluster is
//! one nobody checks.

use std::path::{Path, PathBuf};

use http::Uri;
use kube::config::{AuthInfo, Config, KubeConfigOptions, Kubeconfig};
use kube::Client;

use crate::error::{Error, Result};

/// Where the kubelet projects a Pod's ServiceAccount credentials. Fixed by
/// Kubernetes, not configurable — [`Environment`] holds it as a field only so a
/// test can point at a fixture.
pub const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

const TOKEN: &str = "token";
const CA: &str = "ca.crt";
const NAMESPACE: &str = "namespace";

const SERVICE_HOST: &str = "KUBERNETES_SERVICE_HOST";
const SERVICE_PORT: &str = "KUBERNETES_SERVICE_PORT";

/// Everything [`resolve`] reads.
#[derive(Debug, Clone)]
pub struct Environment {
    pub service_host: Option<String>,
    pub service_port: Option<String>,
    pub service_account_dir: PathBuf,
    /// `KUBECONFIG`, already split. The variable holds a LIST of files that
    /// merge in order, not one path.
    pub kubeconfig: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Environment {
    pub fn from_process() -> Self {
        Self {
            service_host: non_empty(SERVICE_HOST),
            service_port: non_empty(SERVICE_PORT),
            service_account_dir: PathBuf::from(SERVICE_ACCOUNT_DIR),
            kubeconfig: non_empty("KUBECONFIG")
                .map(|v| std::env::split_paths(&v).collect())
                .unwrap_or_default(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

/// An empty variable is an unset one. Kubernetes injects both service variables
/// into every Pod, and a manifest that overrides one with `""` is saying "not
/// this way" — reading that as a hostname produces a URL nothing can dial.
fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Which credential source applies, resolved and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    InCluster {
        url: Uri,
        dir: PathBuf,
    },
    /// The files that exist, in merge order.
    Kubeconfig {
        paths: Vec<PathBuf>,
    },
}

pub fn resolve(env: &Environment) -> Result<Source> {
    // `client-go` requires BOTH variables before it believes it is in a
    // cluster, and so does this: one of them alone is a half-configured
    // manifest, not an apiserver address.
    if let (Some(host), Some(port)) = (&env.service_host, &env.service_port) {
        let token = env.service_account_dir.join(TOKEN);
        // In a Pod with no token, FAIL — do not fall through to a kubeconfig.
        // Falling through is how an agent ends up authenticated against a
        // different cluster than the one it was scheduled in, which is the
        // failure this whole ordering exists to prevent.
        if !token.is_file() {
            return Err(Error::MissingServiceAccountToken(token));
        }
        return Ok(Source::InCluster {
            url: cluster_url(host, port)?,
            dir: env.service_account_dir.clone(),
        });
    }

    let candidates = if env.kubeconfig.is_empty() {
        env.home
            .as_ref()
            .map(|home| vec![home.join(".kube").join("config")])
            .unwrap_or_default()
    } else {
        env.kubeconfig.clone()
    };
    // A named-but-absent entry in `KUBECONFIG` is skipped rather than fatal,
    // which is what kubectl does: the variable is commonly a fixed list of
    // which only some files exist on a given machine.
    let paths: Vec<PathBuf> = candidates.into_iter().filter(|p| p.is_file()).collect();
    if paths.is_empty() {
        return Err(Error::NoCredentials);
    }
    Ok(Source::Kubeconfig { paths })
}

/// `https://host:port`, with an IPv6 literal bracketed — unbracketed it is not
/// a URI at all and the parse below rejects it.
fn cluster_url(host: &str, port: &str) -> Result<Uri> {
    let authority = match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(ip)) => format!("[{ip}]"),
        _ => host.to_string(),
    };
    let url = format!("https://{authority}:{port}");
    url.parse()
        .map_err(|e: http::uri::InvalidUri| Error::MalformedClusterUrl(url, e.to_string()))
}

impl Source {
    /// Read the source's files and produce the configuration a client is built
    /// from.
    pub async fn load(&self) -> Result<Config> {
        match self {
            Source::InCluster { url, dir } => in_cluster(url, dir),
            Source::Kubeconfig { paths } => from_kubeconfig(paths).await,
        }
    }
}

fn in_cluster(url: &Uri, dir: &Path) -> Result<Config> {
    let ca = dir.join(CA);
    // `Config` is `#[non_exhaustive]`, so its own constructor supplies the
    // timeouts and retry policy and this names only what the Pod's own files
    // decide.
    let mut config = Config::new(url.clone());
    config.default_namespace = read(&dir.join(NAMESPACE))?.trim().to_string();
    config.root_cert = Some(pem_certificates(&ca)?);
    // The PATH as well as the bytes: kube re-reads it periodically, so a cluster
    // that rotates its CA does not need this process restarted. Same reason the
    // token goes in as `token_file` — a projected token is refreshed in place,
    // and a copy read once at boot expires within the hour.
    config.root_cert_file = Some(ca);
    config.auth_info = AuthInfo {
        token_file: Some(dir.join(TOKEN).to_string_lossy().into_owned()),
        ..AuthInfo::default()
    };
    Ok(config)
}

async fn from_kubeconfig(paths: &[PathBuf]) -> Result<Config> {
    let mut merged: Option<Kubeconfig> = None;
    for path in paths {
        let next = Kubeconfig::read_from(path)
            .map_err(|e| Error::Kubeconfig(format!("{}: {e}", path.display())))?;
        merged = Some(match merged {
            None => next,
            Some(base) => base
                .merge(next)
                .map_err(|e| Error::Kubeconfig(format!("{}: {e}", path.display())))?,
        });
    }
    let Some(merged) = merged else {
        return Err(Error::NoCredentials);
    };
    Config::from_custom_kubeconfig(merged, &KubeConfigOptions::default())
        .await
        .map_err(|e| Error::Kubeconfig(e.to_string()))
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// The CA bundle as DER, which is what `kube::Config` holds.
fn pem_certificates(path: &Path) -> Result<Vec<Vec<u8>>> {
    let pem = std::fs::read(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let certs: Vec<Vec<u8>> = rustls_pemfile::certs(&mut pem.as_slice())
        .filter_map(|der| der.ok())
        .map(|der| der.to_vec())
        .collect();
    if certs.is_empty() {
        return Err(Error::MalformedCertificate {
            path: path.to_path_buf(),
        });
    }
    Ok(certs)
}

/// A live client, and the namespace its credentials placed it in — the Pod's own
/// for a ServiceAccount, the current context's for a kubeconfig.
///
/// The namespace travels WITH the client because it is part of the same answer:
/// an in-cluster executor works in the namespace it was scheduled into, and a
/// caller that had to go and ask separately would be free to disagree.
pub struct Connection {
    pub client: Client,
    pub namespace: String,
}

/// The one way this crate reaches a cluster: resolve the process environment,
/// load what it names, build a client.
///
/// One entry point on purpose. A second constructor taking a `Config` would let
/// a caller pick a source, and the ordering above is the security property.
pub async fn connect() -> Result<Connection> {
    let source = resolve(&Environment::from_process())?;
    let config = source.load().await?;
    // Which cluster, and on whose say-so. "Why is this agent talking to that
    // apiserver" is the first question a misconfigured executor raises, and the
    // resolution above is the only thing that answers it.
    tracing::info!(
        cluster = %config.cluster_url,
        namespace = %config.default_namespace,
        source = match &source {
            Source::InCluster { .. } => "service-account",
            Source::Kubeconfig { .. } => "kubeconfig",
        },
        "connecting to the Kubernetes API"
    );
    let namespace = config.default_namespace.clone();
    let client = Client::try_from(config).map_err(|e| Error::Client(e.to_string()))?;
    Ok(Connection { client, namespace })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture service-account directory, as the kubelet would project it.
    struct ServiceAccount {
        dir: PathBuf,
    }

    impl ServiceAccount {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nook-k8s-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            Self { dir }
        }

        fn with(self, file: &str, contents: &str) -> Self {
            std::fs::write(self.dir.join(file), contents).expect("fixture file");
            self
        }

        fn complete(name: &str) -> Self {
            Self::new(name)
                .with(TOKEN, "a-projected-token")
                .with(NAMESPACE, "nook-jobs\n")
                .with(CA, CA_PEM)
        }
    }

    impl Drop for ServiceAccount {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A self-signed CA, so the PEM→DER step is asserted against a real
    /// certificate rather than a shape that only looks like one.
    const CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBgjCCASmgAwIBAgIUMnKx5pQ3alJbRwWxPXWNpsj91CIwCgYIKoZIzj0EAwIw
FzEVMBMGA1UEAwwMbm9vay10ZXN0LWNhMB4XDTI2MDgxNzE0Mzg0NFoXDTM2MDgx
NDE0Mzg0NFowFzEVMBMGA1UEAwwMbm9vay10ZXN0LWNhMFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAExfSR1Hv0R+v7G5DLoANDruiprt16z+BAf7GViEdopumavGWJ
jJxAGkheI4NJEMMgM82V2F8LrIRh1m/tQKTV0qNTMFEwHQYDVR0OBBYEFJuAHJK/
GxOI/rb/TGjv3Or6LN22MB8GA1UdIwQYMBaAFJuAHJK/GxOI/rb/TGjv3Or6LN22
MA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDRwAwRAIgTxMRxUHGa6SfpH+b
KHtcoQKoimyDaucAj2OrteH+RroCIDZj/COy7czBM9gFiVgdkIF25KM8JB00JgVM
J2UztCTh
-----END CERTIFICATE-----
";

    fn env(sa: &ServiceAccount) -> Environment {
        Environment {
            service_host: Some("10.96.0.1".into()),
            service_port: Some("443".into()),
            service_account_dir: sa.dir.clone(),
            kubeconfig: Vec::new(),
            home: None,
        }
    }

    /// AC-1: in a Pod, the ServiceAccount is the source.
    #[test]
    fn a_pod_authenticates_with_its_service_account() {
        let sa = ServiceAccount::complete("in-cluster");
        assert_eq!(
            resolve(&env(&sa)).expect("resolves"),
            Source::InCluster {
                url: "https://10.96.0.1:443".parse().unwrap(),
                dir: sa.dir.clone(),
            }
        );
    }

    /// AC-1, the ordering itself. A kubeconfig sitting beside a Pod's projected
    /// token does not win, and cannot: the executor talks to the cluster it is
    /// running in.
    #[test]
    fn a_service_account_beats_a_kubeconfig_that_is_also_present() {
        let sa = ServiceAccount::complete("both");
        let kubeconfig = sa.dir.join("kubeconfig.yaml");
        std::fs::write(&kubeconfig, KUBECONFIG).unwrap();
        let resolved = resolve(&Environment {
            kubeconfig: vec![kubeconfig],
            ..env(&sa)
        })
        .expect("resolves");
        assert!(
            matches!(resolved, Source::InCluster { .. }),
            "a kubeconfig outranked the ServiceAccount: {resolved:?}"
        );
    }

    /// AC-1: outside a cluster, the kubeconfig.
    #[test]
    fn outside_a_cluster_the_kubeconfig_is_the_source() {
        let sa = ServiceAccount::new("outside");
        let path = sa.dir.join("config");
        std::fs::write(&path, KUBECONFIG).unwrap();
        assert_eq!(
            resolve(&Environment {
                service_host: None,
                service_port: None,
                kubeconfig: vec![path.clone()],
                ..env(&sa)
            })
            .expect("resolves"),
            Source::Kubeconfig { paths: vec![path] }
        );
    }

    /// `~/.kube/config` when `KUBECONFIG` says nothing.
    #[test]
    fn an_unset_kubeconfig_variable_falls_back_to_the_home_directory() {
        let sa = ServiceAccount::new("home");
        std::fs::create_dir_all(sa.dir.join(".kube")).unwrap();
        let path = sa.dir.join(".kube").join("config");
        std::fs::write(&path, KUBECONFIG).unwrap();
        assert_eq!(
            resolve(&Environment {
                service_host: None,
                service_port: None,
                home: Some(sa.dir.clone()),
                ..env(&sa)
            })
            .expect("resolves"),
            Source::Kubeconfig { paths: vec![path] }
        );
    }

    /// A `KUBECONFIG` list merges in order, and entries that do not exist are
    /// skipped rather than fatal.
    #[test]
    fn a_kubeconfig_list_keeps_the_files_that_exist_in_order() {
        let sa = ServiceAccount::new("list");
        let first = sa.dir.join("first");
        let third = sa.dir.join("third");
        std::fs::write(&first, KUBECONFIG).unwrap();
        std::fs::write(&third, KUBECONFIG).unwrap();
        assert_eq!(
            resolve(&Environment {
                service_host: None,
                service_port: None,
                kubeconfig: vec![first.clone(), sa.dir.join("absent"), third.clone()],
                ..env(&sa)
            })
            .expect("resolves"),
            Source::Kubeconfig {
                paths: vec![first, third]
            }
        );
    }

    /// Neither source: a named error, not a client pointed at nothing.
    #[test]
    fn neither_source_is_an_error_and_not_a_guess() {
        let sa = ServiceAccount::new("neither");
        let err = resolve(&Environment {
            service_host: None,
            service_port: None,
            home: Some(sa.dir.clone()),
            ..env(&sa)
        })
        .expect_err("no credentials");
        assert!(matches!(err, Error::NoCredentials), "{err}");
    }

    /// Half a manifest is not a cluster. With only the host set, in-cluster
    /// does not apply — and with no kubeconfig either, that is `NoCredentials`
    /// rather than a URL built from a missing port.
    #[test]
    fn one_service_variable_alone_is_not_in_cluster() {
        let sa = ServiceAccount::complete("half");
        let err = resolve(&Environment {
            service_port: None,
            home: None,
            ..env(&sa)
        })
        .expect_err("not in a cluster");
        assert!(matches!(err, Error::NoCredentials), "{err}");
    }

    /// A Pod with the variables and no token FAILS. Falling back to a
    /// kubeconfig here is how an agent authenticates against the wrong cluster.
    #[test]
    fn a_pod_with_no_projected_token_fails_rather_than_falling_back() {
        let sa = ServiceAccount::new("no-token");
        let kubeconfig = sa.dir.join("config");
        std::fs::write(&kubeconfig, KUBECONFIG).unwrap();
        let err = resolve(&Environment {
            kubeconfig: vec![kubeconfig],
            ..env(&sa)
        })
        .expect_err("no token");
        assert!(
            matches!(err, Error::MissingServiceAccountToken(_)),
            "a tokenless Pod fell back instead of failing: {err}"
        );
    }

    /// The in-cluster config carries the Pod's namespace, the CA as DER, and
    /// the token as a PATH so a rotated projection is picked up.
    #[tokio::test]
    async fn the_in_cluster_config_reads_the_projected_files() {
        let sa = ServiceAccount::complete("load");
        let config = resolve(&env(&sa))
            .expect("resolves")
            .load()
            .await
            .expect("loads");
        assert_eq!(config.cluster_url.to_string(), "https://10.96.0.1:443/");
        assert_eq!(config.default_namespace, "nook-jobs");
        assert_eq!(
            config.auth_info.token_file.as_deref(),
            Some(sa.dir.join(TOKEN).to_string_lossy().as_ref()),
            "the token was read once instead of being re-read from its path"
        );
        assert_eq!(
            config.root_cert_file.as_deref(),
            Some(sa.dir.join(CA).as_path())
        );
        assert_eq!(
            config.root_cert.as_ref().map(|c| c.len()),
            Some(1),
            "the PEM CA bundle did not survive as DER"
        );
        assert!(!config.accept_invalid_certs);
    }

    /// A ServiceAccount whose CA is not a certificate is a named failure, not a
    /// client that silently trusts nothing.
    #[tokio::test]
    async fn a_corrupt_ca_bundle_is_named() {
        let sa = ServiceAccount::new("bad-ca")
            .with(TOKEN, "t")
            .with(NAMESPACE, "nook-jobs")
            .with(CA, "not a certificate");
        let err = resolve(&env(&sa))
            .expect("resolves")
            .load()
            .await
            .expect_err("bad CA");
        assert!(matches!(err, Error::MalformedCertificate { .. }), "{err}");
    }

    /// The kubeconfig branch loads through kube's own parser, and the namespace
    /// comes from the current context.
    #[tokio::test]
    async fn the_kubeconfig_config_uses_its_current_context() {
        let sa = ServiceAccount::new("kubeconfig-load");
        let path = sa.dir.join("config");
        std::fs::write(&path, KUBECONFIG).unwrap();
        let config = Source::Kubeconfig { paths: vec![path] }
            .load()
            .await
            .expect("loads");
        assert_eq!(
            config.cluster_url.to_string(),
            "https://cluster.example:6443/"
        );
        assert_eq!(config.default_namespace, "nook-jobs");
    }

    #[test]
    fn an_ipv6_apiserver_is_bracketed() {
        assert_eq!(
            cluster_url("fd00::1", "6443").unwrap().to_string(),
            "https://[fd00::1]:6443/"
        );
    }

    #[test]
    fn a_hostname_apiserver_is_left_alone() {
        assert_eq!(
            cluster_url("kubernetes.default.svc", "443")
                .unwrap()
                .to_string(),
            "https://kubernetes.default.svc:443/"
        );
    }

    /// A port that is not a number. Real manifests carry
    /// `KUBERNETES_SERVICE_PORT=tcp://10.96.0.1:443` — the Docker-link form —
    /// and a URL built from it must be refused rather than dialled.
    #[test]
    fn a_port_that_is_not_a_number_is_named() {
        let err = cluster_url("10.96.0.1", "tcp://10.96.0.1:443").expect_err("rejected");
        assert!(matches!(err, Error::MalformedClusterUrl(..)), "{err}");
    }

    #[test]
    fn a_host_that_is_not_an_address_is_named() {
        let err = cluster_url("not a host", "443").expect_err("rejected");
        assert!(matches!(err, Error::MalformedClusterUrl(..)), "{err}");
    }

    const KUBECONFIG: &str = "apiVersion: v1
kind: Config
current-context: nook
clusters:
  - name: nook
    cluster:
      server: https://cluster.example:6443
contexts:
  - name: nook
    context:
      cluster: nook
      user: nook
      namespace: nook-jobs
users:
  - name: nook
    user:
      token: a-token
";
}
