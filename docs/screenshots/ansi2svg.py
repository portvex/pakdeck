"""Turn the ANSI captures from shoot.sh into SVG screenshots for the README.

Usage: python ansi2svg.py IN_DIR OUT_DIR   (needs the `rich` package)
"""

import io
import sys
from pathlib import Path

from rich.console import Console
from rich.terminal_theme import MONOKAI, TerminalTheme
from rich.text import Text

TITLES = {
    "installed": "pakdeck: installed packages and details",
    "search": "pakdeck: searching the repositories",
    "themes": "pakdeck: theme picker with live preview",
    "terminal": "pakdeck: installing in the embedded terminal",
}


# Cells pakdeck leaves at the terminal's default colours get tokyo-night's
# background and foreground (the theme the shots use), not black.
_ANSI = [tuple(MONOKAI.ansi_colors[i]) for i in range(16)]
THEME = TerminalTheme((26, 27, 38), (192, 202, 245), _ANSI[:8], _ANSI[8:])


def main() -> None:
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    for ansi in sorted(src.glob("*.ansi")):
        lines = ansi.read_text(encoding="utf-8").rstrip("\n").split("\n")
        width = max(len(Text.from_ansi(line).plain) for line in lines)
        console = Console(
            record=True,
            width=width,
            force_terminal=True,
            color_system="truecolor",
            file=io.StringIO(),
        )
        for line in lines:
            console.print(
                Text.from_ansi(line), soft_wrap=True, no_wrap=True, overflow="ignore"
            )
        out = dst / f"{ansi.stem}.svg"
        console.save_svg(str(out), title=TITLES.get(ansi.stem, "pakdeck"), theme=THEME)
        print(f"wrote {out}")


if __name__ == "__main__":
    main()
