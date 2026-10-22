"""Diff two captures of TMR's console output, less what changes from run to run (TODO 29).

The split moves every print of the setup path, which can't run inside a test, so its parity rests
on the owner's A/B: the same command line under the old exe and the new one, each captured, then
diffed here. Both captures are read the same way:
  ticker      frames taken off the screen by their erase codes (`\\r ESC[nA ESC[J`) are dropped,
              then any frame left (a capture that kept no codes) by its lines: `Runtime: ...`
              without a ` | Test ` part, `Test n/m ...`, and the blank line a frame starts with
  ANSI        every other escape sequence (colour, bold) is stripped, and every `\\r`
  masked      log stamps, hh:mm:ss, run stems (`TMR_2026-10-22_19-30-00`), pattern seeds and
              addresses, and what was measured: times (`1.2s`, `340ms`, `01:02.345`), rates
              (`MiB/s`, `ops/s`), op and sample counts (`163.84K ops`, `1000 samples`), and every
              other decimal (`88.5%`, `4.299874 GHz`, `+5.5`, `1.08x`). Kept, as the configuration's:
              byte counts (`8.00 MiB`), whole seconds (`30s`, a test's time limit), a `Wait(500ms)`
              and a cache multiplier (`CacheTotal 2.00x`)
  workers     each run of lines that name a thread (`[Thread 3] Running ...`) is put in thread
              order, each thread's lines in theirs: the scheduler orders them
  tables      a table's cells are joined by ` | ` and its rules cut to `-`, as a masked cell no
              longer sets its column's width; a table of threads (`Thread` its first header) has
              its rows sorted, as a step's thread table orders them by time and its latency table
              by when each thread finished
--mask REGEX masks more (repeatable), e.g. the memory figures of two runs on a busy box. A capture
may be UTF-8 or, as Windows PowerShell's `>` writes it, UTF-16.

Capture stdout and stderr to files of their own, so a line that moves from one stream to the other
shows, and from cmd.exe: Windows PowerShell 5.1 writes a native exe's stderr as error records that
quote the command line.
    cmd /c "tmr.exe --startup-debug memory=50% cputype=cores cpus=100% skip-cores=0 > before.out 2> before.err"
    python tools/console_diff.py before.out after.out [--mask REGEX ...] [--context N]
    python tools/console_diff.py before.err after.err
    python tools/console_diff.py --show before.out      # one capture as it is compared
Exits 0 when the two are the same, 1 when they differ.
"""
import difflib
import re
import sys

# A ticker erase: back to the row n up (none: this row), clear to the end. A capture can turn the
# `\r` into a line end.
ERASE = re.compile(r"\r\n?\x1b\[(?:(\d+)A)?\x1b\[J")
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
TICKER_LINE = re.compile(r"^(Runtime: (?!.* \| Test ).*|Test \d+/\d+ .*)$")
THREAD = re.compile(r"[Tt]hread (\d+)")
# In order: a pattern that contains another's comes first, and a mask leaves no figure for the
# ones after it. A figure starts a word (`P1=72.3` masks the 72.3, not the 1).
FIGURE = r"(?<![\w.])"
MASKS = [
    (r"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(\.\d+)?", "<stamp>"),
    (r"TMR_\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}", "TMR_<stem>"),
    (r"\b\d+:\d{2}:\d{2}\b", "<hms>"),
    (r"\b\d+:\d{2}\.\d{3}\b", "<time>"),
    (r"\bseed 0x[0-9a-f]+", "seed <seed>"),
    (r"address 0x[0-9a-f]+", "address <addr>"),
    (FIGURE + r"[+-]?\d+(\.\d+)?[KMB]? (MiB|GiB|MB|GB|ops)/s\b", "<rate>"),
    # TMR prints a time it measured with decimals (`{:.1}s`); `30s` is a configured limit
    (FIGURE + r"[+-]?\d+\.\d+s\b", "<time>"),
    (FIGURE + r"(?<!Wait\()[+-]?\d+(\.\d+)?(ms|ns)\b", "<time>"),
    (FIGURE + r"\d+(\.\d+)?[KMB]? ops\b", "<ops>"),
    (FIGURE + r"\d+ samples\b", "<n> samples"),
    (FIGURE + r"(?<!CacheTotal )(?<!CacheTotal \()[+-]?\d+\.\d+(?![\d.]|\s?(?:[KMG]i?B|B)\b)", "#"),
]


def read(path):
    """The capture's text, from UTF-8 or UTF-16 (by its byte order mark)."""
    with open(path, "rb") as f:
        raw = f.read()
    if raw.startswith((b"\xff\xfe", b"\xfe\xff")):
        return raw.decode("utf-16")
    return raw.decode("utf-8-sig", errors="replace")


def erase(text):
    """The text with each erased ticker frame gone: an erase cuts back to the start of the row it
    moves the cursor to."""
    out, at = "", 0
    for m in ERASE.finditer(text):
        out += text[at:m.start()]
        cut = len(out)
        for _ in range(int(m.group(1) or 0) + 1):
            cut = out.rfind("\n", 0, cut)
            if cut < 0:
                break
        out = out[:cut + 1]
        at = m.end()
    return out + text[at:]


def tables(lines):
    """The lines with each table (`TableBuilder::print`: a rule, the headers, a rule, the rows, a
    rule) as its cells, a table of threads' rows sorted."""
    def rule(line):
        return len(line) >= 10 and set(line) == {"-"}

    def cells(line):
        return [c.strip() for c in line.split("  ") if c.strip()]

    out, i = [], 0
    while i < len(lines):
        if rule(lines[i]) and i + 2 < len(lines) and rule(lines[i + 2]):
            end = next((j for j in range(i + 3, len(lines)) if rule(lines[j])), len(lines))
            headers = cells(lines[i + 1])
            rows = [" | ".join(cells(line)) for line in lines[i + 3:end]]
            if headers[:1] == ["Thread"]:
                rows.sort()
            out += ["-", " | ".join(headers), "-"] + rows + ["-"]
            i = end + 1
        else:
            out.append(lines[i])
            i += 1
    return out


def normalised(path, extra):
    """The capture's lines as they are compared: no ticker, no escapes, the run's own figures
    masked."""
    text = ANSI.sub("", erase(read(path))).replace("\r", "")
    masks = MASKS + [(p, "<masked>") for p in extra]
    lines = []
    for line in text.split("\n"):
        if TICKER_LINE.match(line):
            if line.startswith("Runtime: ") and lines and not lines[-1]:
                lines.pop()
            continue
        for pattern, mask in masks:
            line = re.sub(pattern, mask, line)
        lines.append(line)
    # The workers' lines come in the order the scheduler gave them: each run of lines that name a
    # thread goes in thread order, each thread's own lines in theirs (as src/golden.rs does)
    at = 0
    while at < len(lines):
        end = at
        while end < len(lines) and THREAD.search(lines[end]):
            end += 1
        lines[at:end] = sorted(lines[at:end], key=lambda line: int(THREAD.search(line).group(1)))
        at = max(end, at + 1)
    return tables(lines)


def main():
    # TMR's glyphs, whatever the console's code page
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    args = sys.argv[1:]
    extra = [args[i + 1] for i, a in enumerate(args) if a == "--mask"]
    context = int(args[args.index("--context") + 1]) if "--context" in args else 3
    files = [a for i, a in enumerate(args)
             if not a.startswith("--") and (i == 0 or args[i - 1] not in ("--mask", "--context"))]
    if "--show" in args and len(files) == 1:
        print("\n".join(normalised(files[0], extra)))
        return
    if len(files) != 2:
        sys.exit(__doc__)
    before, after = (normalised(f, extra) for f in files)
    diff = list(difflib.unified_diff(before, after, files[0], files[1], n=context, lineterm=""))
    if diff:
        print("\n".join(diff))
        sys.exit(1)
    print(f"same: {len(before)} lines after masking")


if __name__ == "__main__":
    main()
