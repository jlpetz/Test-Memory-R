# TODO 111: Reactor, the Log tab following its end, in a monospaced font

Two things the window's Log tab waits on Reactor for (the owner, 2026-10-31: for later; 2026-11-01: both).

## The scroll

The Log tab reads oldest at the top (TODO 109, 97c4bc9), so its newest line is at the bottom, and it should follow it as lines come,
unless the user has scrolled back to read. Reactor (windows-reactor 0.100.0, and its master on 2026-10-31) can't: ScrollViewer and
ScrollView expose only their scroll-bar visibility, with no offset, no scroll-to, no anchoring and no scroll events.

Another user asked for it on 2026-10-30: windows-rs issue 5272, "windows-reactor: expose programmatic scrolling (ScrollViewer
offset/shift, ListView::scroll_into_view, ElementRef::bring_into_view)", open, no replies on 2026-11-01. Its use case is a chat client
opening at the newest message. It proposes, in order: a `vertical_offset()` getter with a one-shot `scroll_to(f64)`; `ListView::
scroll_into_view(index)`; `ElementRef::bring_into_view()`, exposing the native `StartBringIntoView` Reactor already has behind its `test`
feature. Any one serves the Log tab: the first lets it follow the end only while the user is at it (read the offset, scroll if at the
bottom), the others follow it always.

So no issue of our own: our case can go on 5272 as a comment. It posts under the owner's GitHub account, so only on their word. A
draft:

> The same need for a log view that grows: it should follow its newest line while the user is at the end, and hold still once they've
> scrolled back to read (UWP's `KeepLastItemInView`). Proposal 1 covers both halves, since the offset getter tells us whether the user
> is at the end. We moved our lists off ListView over windows-rs issue 5222, so ScrollViewer is the one we'd use.

## The font

The Log tab shows the console's lines as text, and the console's tables line up by padding spaces, which needs a monospaced font. Reactor
0.100.0 has no font family on TextBlock; its master has one (2026-10-31). The report panel needs none: it draws each table as a grid, a
cell a value. The timer's font (TODO 7) comes with the same release.

## Next

On Reactor's next release: take the font for the Log tab (and the timer), and check 5272. Until then, the owner may add our case to it.
