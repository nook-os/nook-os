// jsdom does no layout, so it ships no `scrollIntoView` at all — the property
// is missing rather than a no-op. Any component that keeps a selection visible
// in a scrolling strip therefore throws `not a function` the moment it mounts
// under test, which accuses the component of a bug the browser does not have.
// A no-op is the honest stand-in: there is nothing to scroll here.
if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = function scrollIntoView() {};
}
