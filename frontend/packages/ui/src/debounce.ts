/** A trailing debounce: `fn` runs once, `delay` ms after the LAST call, with
 *  that last call's arguments. Every call inside the window resets the timer.
 *
 *  Pulled out of `SearchInput` so the timing — the part worth getting right,
 *  since it decides how many requests a keystroke storm fires — is unit-testable
 *  without rendering anything. */
export function debounce<A extends unknown[]>(
  fn: (...args: A) => void,
  delay: number,
): (...args: A) => void {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return (...args: A) => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => fn(...args), delay);
  };
}
