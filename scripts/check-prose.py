#!/usr/bin/env python3
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

GLOBS = [
    "README.md",
    "docs/BACKLOG.md",
    "deploy/**/*.md",
    "website/content/**/*.mdx",
    ".claude/skills/writing-blogs/*.md",
]

ENGLISH_WORDS = [
    "delve", "delves", "delving", "tapestry", "intricate", "intricacies", "pivotal",
    "landscape", "foster", "fosters", "testament", "multifaceted", "nuanced", "groundbreaking", "transformative",
    "paradigm", "holistic", "robust", "robustness", "leverage", "leverages", "leveraging", "synergy", "utilize",
    "utilizes", "utilizing", "utilization", "comprehensive", "streamline", "streamlines", "streamlined", "seamless",
    "seamlessly", "crucial", "crucially", "moreover", "furthermore", "additionally", "modern", "cutting-edge",
    "state-of-the-art", "game-changer", "game-changing", "revolutionize", "revolutionizes", "revolutionary",
    "empower", "empowers", "empowering", "unlock", "unlocks", "unleash", "elevate", "elevates", "harness",
    "effortless", "effortlessly", "blazing", "blazingly", "supercharge", "supercharged", "realm", "vibrant",
    "meticulous", "meticulously", "embark", "journey", "notably", "powerful", "next-generation", "world-class",
    "best-in-class", "showcase", "showcases", "boasts", "enhance", "enhances", "enhanced", "facilitate",
    "facilitates", "navigate", "navigating", "plethora", "myriad", "commendable", "invaluable",
]

ENGLISH_PHRASES = [
    "it's worth noting", "it is worth noting", "it's important to note", "it is important to note",
    "when it comes to", "at the end of the day", "in conclusion", "in summary", "dive into", "deep dive",
    "let's dive", "in today's", "ever-evolving", "fast-paced", "plays a crucial role", "a wide range of",
    "not just", "isn't just", "the real unlock",
]

CZECH_WORDS = [
    "moderní", "moderního", "moderním", "robustní", "bezproblémový", "bezproblémová", "bezproblémové",
    "bezproblémově", "revoluční", "inovativní", "špičkový", "špičková", "špičkové", "komplexní",
    "klíčový", "klíčová", "klíčové", "zásadní", "nepostradatelný", "nepostradatelná",
]

CZECH_PHRASES = ["stojí za zmínku", "v neposlední řadě", "je důležité poznamenat", "v dnešní době"]

CHARACTERS = {
    "→": "arrow, write 'to' or rephrase",
    "←": "arrow, rephrase",
    "↔": "arrow, rephrase",
    "⇒": "arrow, rephrase",
    "—": "em dash, use a full stop, a comma or a colon",
    "–": "en dash, write 'to' for ranges or use a comma",
    "…": "ellipsis character, write the sentence out",
    "“": "curly quote, use a straight quote",
    "”": "curly quote, use a straight quote",
    "‘": "curly quote, use a straight quote",
    "’": "curly quote, use a straight apostrophe",
}

CZECH_ALLOWED_CHARACTERS = {"“"}

EMOJI = re.compile("[\U0001F300-\U0001FAFF☀-➿⬀-⯿]")
FENCE = re.compile(r"^\s*(```|~~~)")
INLINE_CODE = re.compile(r"`[^`\n]*`")


def files():
    found = {path for pattern in GLOBS for path in ROOT.glob(pattern) if path.is_file()}
    return sorted(found)


def is_czech(path):
    return path.name.endswith(".cs.mdx") or path.name.endswith(".cs.md")


def prose_lines(text):
    in_fence = False
    for number, line in enumerate(text.splitlines(), start=1):
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if not in_fence:
            yield number, INLINE_CODE.sub("", line)


def word_pattern(words):
    return re.compile(r"(?<![\w-])(" + "|".join(re.escape(word) for word in words) + r")(?![\w-])", re.IGNORECASE)


ENGLISH_WORD = word_pattern(ENGLISH_WORDS)
CZECH_WORD = word_pattern(CZECH_WORDS)


def findings(path):
    czech = is_czech(path)
    words = CZECH_WORD if czech else ENGLISH_WORD
    phrases = CZECH_PHRASES if czech else ENGLISH_PHRASES
    for number, line in prose_lines(path.read_text(encoding="utf-8")):
        lowered = line.lower()
        for match in words.finditer(line):
            yield number, f"word '{match.group(0)}'"
        for phrase in phrases:
            if phrase in lowered:
                yield number, f"phrase '{phrase}'"
        for character, advice in CHARACTERS.items():
            if character in line and not (czech and character in CZECH_ALLOWED_CHARACTERS):
                yield number, f"'{character}' ({advice})"
        for match in EMOJI.finditer(line):
            yield number, f"emoji '{match.group(0)}'"


def main():
    problems = [
        f"{path.relative_to(ROOT)}:{number}: {message}"
        for path in files()
        for number, message in findings(path)
    ]
    if not problems:
        return 0
    print("Prose check failed. Rewrite these in plain words (see .claude/skills/writing-blogs/SKILL.md):", file=sys.stderr)
    print("\n".join(problems), file=sys.stderr)
    print(f"{len(problems)} problem(s).", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
