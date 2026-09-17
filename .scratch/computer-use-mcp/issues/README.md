# Ticket Index — Computer Use MCP

Status: Approved and published. GitHub Issues in `Klyve-09/computer_use` are the authoritative tracker; these files preserve the local handoff copy.

| Ticket | End-to-end result | Blocked by |
|---|---|---|
| [#2 — 01: Observe a selected Monitor from Codex](https://github.com/Klyve-09/computer_use/issues/2) | Codex lists Monitors and receives a selected screenshot with usable Observation metadata. | None |
| [#3 — 02: Click and scroll on mixed-scale Monitors](https://github.com/Klyve-09/computer_use/issues/3) | Codex targets observed pixels, clicks/scrolls, and sees the result. | #2 |
| [#4 — 03: Enter addresses and Korean text through Codex](https://github.com/Klyve-09/computer_use/issues/4) | The browser workflow and Unicode/multiline text entry work end to end. | #3 |
| [#5 — 04: Drag within and between mixed-scale Monitors](https://github.com/Klyve-09/computer_use/issues/5) | One Action drags between independently scaled Monitor images and observes the destination. | #3 |
| [#6 — 05: Recover from display changes and interrupted actions](https://github.com/Klyve-09/computer_use/issues/6) | Codex recovers from display changes/interruption and resumes through fresh observations. | #4, #5 |

Tickets 03 and 04 can proceed independently after 02. No unnecessary edge joins them. Each ticket includes its own behavior verification; 05 adds runtime recovery rather than deferring all testing until the end.

The approved [specification is issue #1](https://github.com/Klyve-09/computer_use/issues/1). All five tickets carry `ready-for-agent`; native GitHub blocking relationships are attached as shown above. The source spec remains open.
