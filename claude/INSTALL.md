# Wiring inkling into Claude Code

Deliberately outside the skill, so the skill stays a copy of one file:
`inkling-skill.md` here is the source, and each release copies it into the
`inkling` plugin in [dpep/claude](https://github.com/dpep/claude).

## The plugin (recommended)

```
/plugin marketplace add dpep/claude
/plugin install inkling@dpep
```

That brings the four hooks and the skill. The plugin needs the binary on PATH:

```sh
cargo install --git https://github.com/dpep/inkling
```

## By hand

Install the binary as above, add the hooks from the README's Quick start to
`~/.claude/settings.json`, and copy `inkling-skill.md` to
`~/.claude/skills/inkling/SKILL.md`. Don't do both: with the plugin enabled as
well, every hook fires twice.
