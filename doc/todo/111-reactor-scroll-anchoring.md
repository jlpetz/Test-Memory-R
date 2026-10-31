# TODO 111: ask Reactor for a way to follow a list's end

The window's Log tab reads oldest at the top (TODO 109, 97c4bc9), so its newest line is at the bottom, and it should follow it as lines
come, unless the user has scrolled back to read. Reactor (windows-reactor 0.100.0, and its master on 2026-10-31) can't: ScrollViewer and
ScrollView expose only their scroll-bar visibility, with no offset, no scroll-to, no anchoring and no scroll events. The owner,
2026-10-31: a TODO for later, not posted yet.

## The draft (posts under the owner's GitHub account, so only on their word)

> **Reactor: a way to keep a growing list at its end (ScrollViewer anchoring or scroll-to)**
>
> A log view and a results list that grow over time should follow their newest item, unless the user has scrolled back to read. In
> 0.100.0, and on master today, ScrollViewer and ScrollView expose only their scroll-bar visibility. There's no scroll offset,
> `ChangeView`/scroll-to, or anchoring, and no scroll events, so an app can't follow the end or tell whether the user scrolled. Any one
> of these would do:
> - `ScrollViewer.VerticalAnchorRatio` (1.0 keeps the view at the end while it's there, and holds it once the user scrolls up)
> - a scroll-to on the element reference
> - ListView's `ItemsUpdatingScrollMode::KeepLastItemInView` (we moved off ListView over windows-rs issue 5222)

## Before posting

Check Reactor's latest release and master for any of the three first: the request is moot if one has landed. Once one is there, the Log
tab follows its end with it, and the monospace font (TextBlock FontFamily, on master 2026-10-31) can come in the same Reactor update.
