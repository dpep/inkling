# Wiring inkling into Claude Code

Inkling runs on macOS and Linux. Install the binary first; everything below
needs it on `PATH`:

```sh
brew install dpep/tools/inkling   # or: cargo install inkling-dev
```

## The plugin (recommended)

```
/plugin marketplace add dpep/claude
/plugin install inkling@dpep
```

That brings the four hooks and the skill.

## By hand

1. Add the hooks from the README's Quick start to `~/.claude/settings.json`.
2. Copy `inkling-skill.md` to `~/.claude/skills/inkling/SKILL.md`.

Pick one way, not both. With the plugin enabled as well, every hook fires
twice.

## Why the skill lives here

`inkling-skill.md` is the source of truth for the skill. Each release copies it
into the `inkling` plugin in [dpep/claude](https://github.com/dpep/claude), so
edit it here, never there.
