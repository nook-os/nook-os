// jsdom does no layout, so it ships no `scrollIntoView` at all — the property
// is missing rather than a no-op. Any component that keeps a selection visible
// in a scrolling strip therefore throws `not a function` the moment it mounts
// under test, which accuses the component of a bug the browser does not have.
// A no-op is the honest stand-in: there is nothing to scroll here.
if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = function scrollIntoView() {};
}
// jsdom does no layout, so `Range` — which a browser measures text with — has
// neither `getClientRects` nor `getBoundingClientRect`. CodeMirror measures the
// document on an animation frame after every mount, so ANY test rendering the
// markdown editor throws `getClientRects is not a function` from a timer, well
// after the assertion that passed. Empty geometry is the honest stand-in: there
// is no layout here, and CodeMirror already handles a zero-size measurement by
// deferring rather than by misplacing anything.
if (!Range.prototype.getClientRects) {
  Range.prototype.getClientRects = function getClientRects() {
    return Object.assign([], { item: () => null }) as unknown as DOMRectList;
  };
  Range.prototype.getBoundingClientRect = function getBoundingClientRect() {
    return new DOMRect(0, 0, 0, 0);
  };
}
