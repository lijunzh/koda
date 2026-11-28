# Reviewing changes

**Changes** opens as a pane in the side panel (Shift+Cmd+D, the side
panel's list, or a file in the Activity panel's Changes card) with what a
turn changed. Each file is a row with
its added and removed counts; click the row to fold it. Unchanged lines
between the changes are folded to a count, and a bar at the left marks each
added or removed line. While a turn runs the pane updates as Koda edits;
for an earlier turn, pick it from the **Turn** menu at the top.

To comment, hover a line and click the **+** beside its number. Enter adds
the comment. Cmd+Enter, or **Send**, sends all your comments to Koda as one
message, each with its file, line number and the line itself. If a turn is
running, they're added to it; otherwise they start the next turn.

The pane shows the edits Koda's Edit and Write tools made. Files a command
changed (`sed`, a formatter, `git checkout`) and a sub-agent's edits aren't
in it, as in Codex's turn diff.
