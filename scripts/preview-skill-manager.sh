#!/usr/bin/env bash
set -euo pipefail

# Run the real manager with disposable skills and app data.
monocode_preview_repo="$(cd "$(dirname "$0")/.." && pwd)"
if [[ -n "${MONOCODE_PREVIEW_ROOT:-}" ]]; then
    monocode_preview_root="$MONOCODE_PREVIEW_ROOT"
    mkdir "$monocode_preview_root"
else
    monocode_preview_root="$(mktemp -d "${TMPDIR:-/tmp}/monocode-skill-manager-preview.XXXXXX")"
fi
monocode_preview_library_target="${MONOCODE_PREVIEW_LIBRARY_TARGET:-${TMPDIR:-/tmp}/monocode-skill-manager-library-target}"
monocode_preview_binary="${MONOCODE_PREVIEW_BINARY:-$monocode_preview_repo/target/debug/monocode-app}"
monocode_preview_manage="${MONOCODE_PREVIEW_MANAGE:-$monocode_preview_library_target/debug/examples/manage}"

python3 - "$monocode_preview_root" <<'PY'
from pathlib import Path
import json
import sys

root = Path(sys.argv[1])
for directory in ["data", "home", "project", "sources"]:
    (root / directory).mkdir()
(root / "data" / "local-storage.json").write_text(json.dumps({
    "version": 1,
    "items": {
        "monocode.settingsSection": "skills",
        "monocode.projectRailOpen": "false",
    },
}))

skills = {
    "release-check": (
        "Check a release before publishing it.",
        "# Release check\n\nRead references/checklist.md before preparing a release.\n"
        "Run scripts/check.sh from this skill directory. Record any failed check.\n",
        {"references/checklist.md": "# Checklist\n\n- Run the project tests.\n- Review the change log.\n",
         "scripts/check.sh": "#!/bin/sh\nprintf '%s\\n' 'Preview check passed'\n"},
    ),
    "debug-session": (
        "Collect a reproducible failure before changing code.",
        "# Debug session\n\nRead references/repro.md and record the smallest failing case.\n"
        "Run the case again after the fix.\n",
        {"references/repro.md": "# Reproduction\n\nRecord the command, expected result, and observed result.\n"},
    ),
}
for name, (description, body, resources) in skills.items():
    source = root / "sources" / name
    source.mkdir()
    (source / "SKILL.md").write_text(f"---\nname: {name}\ndescription: {description}\n---\n\n{body}")
    for relative, text in resources.items():
        path = source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        if relative.endswith(".sh"):
            path.chmod(0o755)

# Preserve a provider's independent copy so the preview shows a real conflict.
existing = root / "home" / ".claude" / "skills" / "release-check"
existing.mkdir(parents=True)
(existing / "SKILL.md").write_text(
    "---\nname: release-check\ndescription: A provider copy with local edits.\n---\n\n"
    "# Local release check\n\nKeep this team's additional deployment checks.\n"
)
PY

cd "$monocode_preview_repo"
if [[ -z "${MONOCODE_PREVIEW_MANAGE:-}" ]]; then
    cargo build -p monocode-skills --example manage --locked --target-dir "$monocode_preview_library_target"
fi
for monocode_preview_source in "$monocode_preview_root"/sources/*; do
    monocode_preview_name="$(basename "$monocode_preview_source")"
    "$monocode_preview_manage" \
        "$monocode_preview_root/data" "$monocode_preview_root/home" \
        import "$monocode_preview_source" > "$monocode_preview_root/$monocode_preview_name-import-result.json"
done

printf 'Preview data: %s\n' "$monocode_preview_root"
if [[ "${1:-}" == "--prepare-only" ]]; then
    exit 0
fi

if [[ ! -x "$monocode_preview_binary" ]]; then
    cargo build -p monocode-app --bin monocode-app --features screenshot --locked -j 2
fi

cd "$monocode_preview_root/project"
exec "$monocode_preview_binary" --view page-settings \
    --data-dir "$monocode_preview_root/data" --skills-home "$monocode_preview_root/home" "$@"
