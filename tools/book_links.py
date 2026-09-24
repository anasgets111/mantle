#!/usr/bin/env python3
"""mdBook preprocessor: a page's relative link out of `docs/` becomes a GitHub link, and each
```` ```lua,shot ```` block is followed by its screenshot, `images/<page>-<n>.png`.

Pages link code as `../../renderer/...` so they read right on GitHub; the site serves only `docs/`,
where those paths 404. A target that does not exist fails the build, which is the link check for
everything outside the book.
"""

import json
import os
import posixpath
import re
import sys

REPO_URL = "https://github.com/anasgets111/mantle"
LINK = re.compile(r"\]\(([^)\s#]*)(#[^)\s]*)?\)")


def main():
    if len(sys.argv) > 1:  # `supports <renderer>`: every renderer.
        return 0
    context, book = json.load(sys.stdin)
    repo = os.path.dirname(os.path.abspath(context["root"]))
    missing = []

    def rewrite(page, text):
        def replace(match):
            target, fragment = match.group(1), match.group(2) or ""
            if not target.startswith("../"):
                return match.group(0)
            path = posixpath.normpath(posixpath.join("docs", posixpath.dirname(page), target))
            if path.startswith("docs/"):
                return match.group(0)
            full = os.path.join(repo, path)
            if not os.path.exists(full):
                missing.append(f"{page}: {target}")
            kind = "tree" if os.path.isdir(full) else "blob"
            return f"]({REPO_URL}/{kind}/main/{path}{fragment})"

        out, fenced, shot, shots = [], False, False, 0
        stem = posixpath.splitext(page)[0]
        up = "../" * page.count("/")
        for line in text.split("\n"):
            fence = line.lstrip().startswith("```")
            if fence:
                fenced = not fenced
                shot = shot or line.strip() == "```lua,shot"
            out.append(line if fenced or fence else LINK.sub(replace, line))
            if fence and not fenced and shot:
                shots, shot = shots + 1, False
                out.append(f"\n![What the example above draws]({up}images/{stem}-{shots}.png)")
        return "\n".join(out)

    def walk(node):
        if isinstance(node, dict):
            chapter = node.get("Chapter")
            if chapter and chapter.get("path"):
                chapter["content"] = rewrite(chapter["path"], chapter["content"])
            for value in node.values():
                walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    walk(book)
    if missing:
        sys.stderr.write("links to files that do not exist:\n  " + "\n  ".join(missing) + "\n")
        return 1
    json.dump(book, sys.stdout)
    return 0


sys.exit(main())
