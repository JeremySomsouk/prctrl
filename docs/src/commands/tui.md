# tui

Browse pull requests in a responsive terminal review desk.

![Review desk with synthetic example data](../assets/tui-review-desk.png)

```bash
prctrl                      # Launch the default TUI
prctrl tui                  # Explicit equivalent
prctrl tui --interval 60
prctrl tui --interval 0
NO_COLOR=1 prctrl tui
```

The default refresh interval is 30 seconds. Zero disables automatic refresh. GitHub loads run in the background; navigation and quit remain available during a sync. Old results remain visible if a refresh fails. Press `r` to retry.

## Navigation

Use `Tab` / `Shift+Tab` or `1`–`5` for Reviews, Mine, Crew, Stats and Live. Crew uses configured crew members, or the Reviews dataset when none are configured. Stats summarize the Reviews dataset before the local filter. Entering Live requests a fresh sync. Other views reuse snapshots for up to 60 seconds.

Use arrow keys or `j` / `k` to move through PRs, `PageUp` / `PageDown` for ten rows, and `Home` / `End` for the first or last filtered result. A details panel shows the selected PR when space permits. Compact terminals replace the sidebar with numbered tabs. Terminals below 30 columns or 10 rows show a resize hint.

## Filtering and actions

Press `/` and type a title, author, repository or PR number. Shortcut letters, including `q` and `r`, are ordinary text during editing. `Enter` applies the filter; `Esc` restores the previous query. Outside editing, `Esc` or `Ctrl+F` clears the filter.

Press `o` to request browser launch, or `Enter` to open the action menu. The TUI offers only implemented actions; approval, review and diff commands remain available in the CLI.

Press `r` or `Ctrl+R` to request fresh data. Press `?` for help. `Esc` closes panels and dismisses notices. `q` or `Ctrl+C` exits, with terminal settings restored.

## Accessibility

Selection has a `>` marker. `DRAFT`, `OPEN` and `OVERDUE` text labels complement the colors; `OVERDUE` means more than seven days old. Set `NO_COLOR=1` to use terminal defaults and monochrome selection. Unicode titles remain supported.
