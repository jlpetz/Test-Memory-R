"""Compare the compiled Rust candidate generators against independent Python."""
from pathlib import Path
import subprocess
import simulate as model

ROOT = Path(__file__).resolve().parents[2]
binary = ROOT / "target/pattern-audit-probe/cpu_probe.exe"
dump = subprocess.check_output([str(binary), "--dump-lines"], text=True)
count = 0
for line in dump.splitlines():
    name, address_hex, values = line.split(" ", 2)
    address = int(address_hex, 16)
    actual = [int(v.strip(), 16) for v in values.strip("[]").split(",")]
    seed = model.splitmix((address >> 6) + model.KEY)
    expected = {
        "cheap": lambda: [model.cheap_word(address + i * 8) for i in range(8)],
        "seal": lambda: [model.seal_word(address + i * 8) for i in range(8)],
        "tagged": lambda: [model.seal_word(address + i * 8, tagged=True, key=model.KEY) for i in range(8)],
        "near32": lambda: model.near_flip_line_from_seed(seed, 32),
        "near64": lambda: model.near_flip_line_from_seed(seed, 64),
        "triple": lambda: model.triple_line(address),
        "header32": lambda: model.header_flip_from_seed(seed, 32),
        "header64": lambda: model.header_flip_from_seed(seed, 64),
    }[name]()
    assert actual == expected, (name, address_hex, actual, expected)
    count += 1
print(f"Independent Python/Rust agreement: {count} complete 64-byte lines.")
