"""Portable selections for the launcher's pinned optional Nix outputs."""

LANGUAGE_SERVERS = "Language servers"

PACKAGE_CHOICES = [
    {"id": "basic-tools", "label": "Basic CLI tools", "group": "Tools",
     "description": "Everyday shell, archive, search, network and diagnostic tools.", "default": True},
    {"id": "chromium", "label": "Chromium", "group": "Apps",
     "description": "Browser running inside the private container.", "default": True},
    {"id": "obsidian", "label": "Obsidian (unfree)", "group": "Apps",
     "description": "Proprietary Markdown knowledge-base application.", "default": False},
    {"id": "gh", "label": "GitHub CLI", "group": "Tools",
     "description": "GitHub command-line client.", "default": True},
    {"id": "ssh", "label": "OpenSSH", "group": "Tools",
     "description": "SSH client and related tools; network permissions still apply.", "default": True},
    {"id": "marksman", "label": "Marksman", "group": LANGUAGE_SERVERS,
     "description": "Markdown language server.", "default": True},
    {"id": "emmet", "label": "Emmet", "group": LANGUAGE_SERVERS,
     "description": "Emmet abbreviation language server.", "default": True},
    {"id": "typescript-lsp", "label": "TypeScript language server", "group": LANGUAGE_SERVERS,
     "description": "JavaScript and TypeScript language support.", "default": True},
    {"id": "typescript", "label": "TypeScript compiler", "group": "Tools",
     "description": "TypeScript compiler (tsc).", "default": True},
    {"id": "html-lsp", "label": "HTML and related VS Code servers", "group": LANGUAGE_SERVERS,
     "description": "Extracted VS Code HTML, CSS, JSON and ESLint language servers.", "default": True},
    {"id": "nil", "label": "nil", "group": LANGUAGE_SERVERS,
     "description": "Nix language server.", "default": True},
    {"id": "nixd", "label": "nixd", "group": LANGUAGE_SERVERS,
     "description": "Nix language server with evaluation support.", "default": True},
    {"id": "java-lsp", "label": "Java language server", "group": LANGUAGE_SERVERS,
     "description": "Eclipse JDT language server, including its JDK.", "default": True},
]
DEFAULT_PACKAGES = sorted(choice["id"] for choice in PACKAGE_CHOICES if choice["default"])
_PACKAGE_IDS = frozenset(choice["id"] for choice in PACKAGE_CHOICES)


def validate_packages(value) -> list[str]:
    if (not isinstance(value, list)
            or any(not isinstance(item, str) or item not in _PACKAGE_IDS for item in value)):
        raise ValueError("Selected packages must be a list of known package IDs")
    return sorted(set(value))
