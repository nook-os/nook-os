import { afterEach, describe, expect, it, vi } from "vitest";
import { debounce } from "./debounce";

afterEach(() => vi.useRealTimers());

describe("debounce", () => {
  it("does not fire before the delay elapses", () => {
    vi.useFakeTimers();
    const spy = vi.fn();
    const d = debounce(spy, 300);
    d("a");
    vi.advanceTimersByTime(299);
    expect(spy).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("collapses a burst into one call with the latest arguments", () => {
    vi.useFakeTimers();
    const spy = vi.fn();
    const d = debounce(spy, 300);
    d("a");
    d("b");
    d("c");
    vi.advanceTimersByTime(300);
    expect(spy).toHaveBeenCalledTimes(1);
    expect(spy).toHaveBeenCalledWith("c");
  });

  it("fires again for a call made after the window closed", () => {
    vi.useFakeTimers();
    const spy = vi.fn();
    const d = debounce(spy, 100);
    d("first");
    vi.advanceTimersByTime(100);
    d("second");
    vi.advanceTimersByTime(100);
    expect(spy).toHaveBeenCalledTimes(2);
    expect(spy).toHaveBeenNthCalledWith(1, "first");
    expect(spy).toHaveBeenNthCalledWith(2, "second");
  });
});
