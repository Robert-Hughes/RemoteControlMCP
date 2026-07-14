- Include in GENERAL.md
  - 
- Include in LOCAL.md
  - Warning about modifyiing Remote Control MCP source while it is running (locked target exe)
  - Stuff copied from skill ChatGPT - delete that skill
  - Suggest improvements to the MCP tools if agent finds them awkward to use or struggles




# Manta Remote Control

Use this skill when operating Rob's Windows PC, named Manta, through the
Remote Control MCP.

Do not restate the MCP tool definitions or explain ordinary tool parameters.
Focus on Manta-specific knowledge, reliable Windows command patterns, coding
agent invocation, allowance measurement and operational judgement.

## Local coding-agent authorisation

Never launch Codex, Antigravity or another local coding agent unless Rob has
explicitly instructed ChatGPT to use one.

Examples that authorise delegation:

- "Use Codex to implement this."
- "Run agy on this task."
- "Ask a local coding agent to investigate."
- "Delegate this to Antigravity."
- "Use a coding agent to fix it."

A general request such as "fix this bug", "review this code" or "make this
change" does not authorise use of a local coding agent.

It is acceptable to suggest that a local agent would be useful, but wait for
confirmation before launching it.

Once Rob authorises a local coding agent:

- use unrestricted or yolo mode by default;
- do not ask for separate permission to disable sandboxing or approvals;
- do not run two writable agents against the same workspace concurrently;
- follow the user's choice when they name Codex or Antigravity.

## Manta environment

Computer name:

`MANTA`

Use PowerShell Core 7:

`pwsh.exe`

Do not use legacy Windows PowerShell:

`powershell.exe`

Remote Control MCP temporary area:

`T:\Temp\RemoteControlMCP`

Codex CLI:

`C:\Users\xman2\AppData\Roaming\npm\codex.cmd`

Antigravity CLI:

`C:\Users\xman2\AppData\Local\agy\bin\agy.exe`

Codex persisted sessions:

`C:\Users\xman2\.codex\sessions`

Antigravity settings:

`C:\Users\xman2\.gemini\antigravity-cli\settings.json`

Antigravity conversations:

`C:\Users\xman2\.gemini\antigravity-cli\conversations`

Antigravity logs:

`C:\Users\xman2\.gemini\antigravity-cli\log`

Use absolute executable paths for important tools. Processes launched by the
MCP may not inherit the same PATH as an interactive terminal.

Set the process working directory to the exact project or workspace directory.
Do not use a drive root when a narrower directory is known.

## General operating approach

Before a substantial operation, verify that the Remote Control MCP responds
unless it has already responded during the current exchange.

For multi-step work:

- retain process IDs and all stdout, stderr and agent-log paths;
- provide brief progress updates while work is running;
- inspect results rather than trusting a process exit code or agent report;
- report failed attempts, retries and corrective passes;
- distinguish agent launches from ordinary validation commands.

Do not commit unless Rob explicitly requests a commit.

After changing a Git repository, inspect:

```powershell
git -C 'D:\path\to\repository' status --short --branch
git -C 'D:\path\to\repository' diff --stat
git -C 'D:\path\to\repository' diff
```

Confirm that no unrelated files changed.

## Long prompts

Do not place a large or multiline coding prompt directly in a process command
line.

Large Windows command lines are fragile because of:

- finite command-line length limits;
- nested parsing through JSON, MCP, PowerShell, batch files and the agent CLI;
- quoting differences between PowerShell, `.cmd` wrappers and native programs;
- embedded newlines, quotes, backticks, dollar signs, braces and backslashes;
- difficult diagnosis when an argument is truncated or transformed.

Write the complete prompt as UTF-8 to a uniquely named file beneath:

`T:\Temp\RemoteControlMCP`

Use a GUID or other unique run identifier in filenames.

For Codex, pipe the prompt file to stdin.

For Antigravity, pass a short command-line prompt telling it to read and follow
the complete prompt file.

Delete temporary prompt and helper files after reporting and diagnosis are
complete.

## Development method

Prefer red/green development.

For a bug fix:

1. Inspect the implementation and existing tests.
2. Add or amend a focused regression test expressing the required behaviour.
3. Run the focused test and confirm that it fails for the expected reason.
4. Change the implementation.
5. Run the focused test again and confirm that it passes.
6. Run the relevant wider test, formatting, lint and build checks.
7. Inspect the final diff.

Do not write the implementation first and then describe the later test as
test-driven.

Do not weaken an existing assertion merely to make the test pass.

When no practical failing automated test can be produced, establish the
closest deterministic reproduction before changing the implementation and
explain the limitation.

For a new feature, prefer test-first development where the codebase has a
suitable test seam.

Include this expectation in coding-agent prompts.

## Choosing an agent

Prefer Codex for:

- established Git repositories;
- implementation, test and review workflows;
- reliable structured JSONL progress;
- a separately captured final response;
- straightforward per-turn token accounting.

Prefer Antigravity when:

- Rob specifically requests Antigravity;
- continuing an existing Antigravity conversation;
- creating a rapid self-contained prototype;
- comparing another agent's approach.

Do not silently substitute one agent for the other.

## Allowance reporting principles

Rob is primarily interested in the percentage of the account allowance
consumed, not raw token totals.

After every authorised local coding-agent run, report allowance information
first.

Report:

- allowance window or quota bucket;
- percentage used or remaining before the run;
- percentage used or remaining after the run;
- percentage-point change attributable to the run;
- reset time;
- whether the measurement is exact, rounded or potentially affected by other
  concurrent activity.

Token counts are secondary supporting information.

Allowance values are account-wide. If another process uses the same Codex or
Antigravity account during measurement, the observed change can include that
activity. Avoid concurrent agent runs while measuring consumption.

If a quota resets during the run, report the old and new windows separately.
Do not calculate a misleading cross-reset percentage-point delta.

## Running Codex

Use non-interactive `codex exec`.

Codex unrestricted mode is:

`--yolo`

It is an alias for bypassing approvals and sandboxing.

Typical PowerShell 7 wrapper:

```powershell
$codex = 'C:\Users\xman2\AppData\Roaming\npm\codex.cmd'
$workspace = 'D:\path\to\repository'

$runId = [guid]::NewGuid().ToString('N')
$promptPath = "T:\Temp\RemoteControlMCP\codex-$runId-prompt.md"
$eventsPath = "T:\Temp\RemoteControlMCP\codex-$runId-events.jsonl"
$stderrPath = "T:\Temp\RemoteControlMCP\codex-$runId-stderr.log"
$finalPath = "T:\Temp\RemoteControlMCP\codex-$runId-final.md"

Get-Content -Raw -LiteralPath $promptPath |
    & $codex exec `
        --yolo `
        --json `
        -C $workspace `
        --output-last-message $finalPath `
        - `
        2> $stderrPath |
    Tee-Object -FilePath $eventsPath
```

Use `--skip-git-repo-check` only when the work deliberately occurs outside a
Git repository.

Do not use `--ephemeral` when persisted rollout information will be needed for
allowance inspection.

Tell Codex explicitly:

- the exact requested scope;
- whether committing is allowed;
- not to modify unrelated files;
- to use red/green development for bugs;
- which validation it must run;
- what its final report must contain.

Suggested ending for an implementation prompt:

```text
For a bug fix, first add a focused regression test and run it to demonstrate
the expected failure. Then implement the fix and show that the focused test
passes. Run the relevant broader checks afterwards.

Do not modify unrelated files. Do not commit unless explicitly requested.

Finish with:
- root cause;
- files changed;
- red test and original failure;
- green test result;
- broader validation;
- remaining concerns.
```

## Measuring Codex allowance consumption

Codex rollout files contain rate-limit snapshots beneath:

`C:\Users\xman2\.codex\sessions`

Relevant fields include:

```text
rate_limits.primary.used_percent
rate_limits.primary.window_minutes
rate_limits.primary.resets_at
```

A secondary limit can also be present:

```text
rate_limits.secondary.used_percent
rate_limits.secondary.window_minutes
rate_limits.secondary.resets_at
```

### Before the run

Immediately before launching Codex:

1. Search recent rollout JSONL files for the newest event containing
   `rate_limits`.
2. Record the primary and secondary values.
3. Record the event timestamp and source file.
4. Record each window's reset timestamp.

This is the baseline.

If the newest snapshot is stale, say so. Do not present a stale baseline as a
precise immediate reading.

### After the run

Parse the Codex JSONL event stream for `thread.started` and obtain the thread
ID where available.

Locate the matching rollout file by thread ID. If the filename cannot be
matched directly, use the newest rollout created after the recorded run start
and verify its contents against the event stream.

Read the last rate-limit snapshot in that rollout.

For each unchanged allowance window:

```text
percentage points consumed =
    after.used_percent - before.used_percent

remaining after =
    100 - after.used_percent
```

Report primary and secondary windows separately.

Example:

```text
Codex allowance

Primary five-hour window:
- Before: 34.0% used
- After: 36.0% used
- Consumed: 2.0 percentage points
- Remaining: 64.0%
- Resets: 14 July 2026 at 13:42

Secondary weekly window:
- Before: 11.0% used
- After: 11.4% used
- Consumed: 0.4 percentage points
- Remaining: 88.6%
- Resets: 18 July 2026 at 09:00
```

Codex percentages may be rounded. If exact tokens increased but the displayed
allowance percentage did not move, report:

```text
Visible allowance change: less than the displayed percentage resolution
```

If another Codex session ran concurrently, label the percentage delta as
account-wide rather than solely attributable to this task.

### Secondary Codex token reporting

The authoritative completed-turn token record is the final event whose type is:

`turn.completed`

Its `usage` object can contain:

- `input_tokens`
- `cached_input_tokens`
- `output_tokens`
- `reasoning_output_tokens`

Accounting rules:

- cached input is included within input;
- reasoning output is included within output;
- total tokens are input plus output.

PowerShell parser:

```powershell
$completed = Get-Content -LiteralPath $eventsPath |
    ForEach-Object {
        try {
            $_ | ConvertFrom-Json -Depth 100
        } catch {
            # Ignore non-JSON diagnostic lines.
        }
    } |
    Where-Object { $_.type -eq 'turn.completed' } |
    Select-Object -Last 1

if ($null -ne $completed.usage) {
    $usage = $completed.usage

    [pscustomobject]@{
        InputTokens           = [long]$usage.input_tokens
        CachedInputTokens     = [long]$usage.cached_input_tokens
        OutputTokens          = [long]$usage.output_tokens
        ReasoningOutputTokens = [long]$usage.reasoning_output_tokens
        TotalTokens           = (
            [long]$usage.input_tokens +
            [long]$usage.output_tokens
        )
    }
}
```

For an interrupted or failed run, label any token information recovered from
rollout events as partial.

Do not claim that a failed run consumed nothing merely because it lacks a
`turn.completed` event.

## Running Antigravity

Use Antigravity's non-interactive print mode.

For unrestricted implementation work:

- include `--dangerously-skip-permissions`;
- use `--mode accept-edits`;
- omit `--sandbox`.

Typical PowerShell 7 wrapper:

```powershell
$agy = 'C:\Users\xman2\AppData\Local\agy\bin\agy.exe'
$workspace = 'D:\path\to\repository'

$runId = [guid]::NewGuid().ToString('N')
$promptPath = "T:\Temp\RemoteControlMCP\agy-$runId-prompt.md"
$stdoutPath = "T:\Temp\RemoteControlMCP\agy-$runId-stdout.log"
$stderrPath = "T:\Temp\RemoteControlMCP\agy-$runId-stderr.log"
$agyLogPath = "T:\Temp\RemoteControlMCP\agy-$runId-cli.log"

$shortPrompt = @"
Read the complete task from:
$promptPath

Follow that file exactly. Work directly in the current workspace. Do not use
an artifact output path as a substitute for editing workspace files.
"@

& $agy `
    --print $shortPrompt `
    --mode accept-edits `
    --dangerously-skip-permissions `
    --print-timeout 30m `
    --log-file $agyLogPath `
    1> $stdoutPath `
    2> $stderrPath
```

Set the MCP process working directory to `$workspace`.

Starting Antigravity at a drive root can cause incorrect project discovery and
output-path behaviour.

Tell Antigravity to edit the real workspace using file or terminal tools.
Its artifact mechanism may reject absolute Windows paths outside its internal
artifact directory.

Use `--continue` only when intentionally continuing the most recent
conversation in the same workspace.

Use `--conversation <id>` when a particular conversation must be resumed.

If no `--model` option is supplied, Antigravity uses its configured model.

Report the model found in the status payload or run log.

## Measuring Antigravity allowance consumption

Antigravity provides a supported custom-statusline JSON interface.

The status payload includes:

- `conversation_id`;
- `model`;
- `context_window`;
- `quota`;
- `plan_tier`;
- `agent_state`;
- `sandbox`.

Each quota bucket can contain:

- `remaining_fraction`;
- `reset_time`;
- `reset_in_seconds`;
- `disabled`.

Typical bucket names include:

- `gemini-5h`
- `gemini-weekly`
- `3p-5h`
- `3p-weekly`

### Temporary statusline capture

Before changing Antigravity settings:

1. Copy the settings file byte-for-byte to a unique backup.
2. calculate its SHA-256 hash;
3. install a temporary statusline command;
4. restore the original file in a `finally` block;
5. verify that the restored file hash matches the backup.

Do not leave the capture hook installed after the task.

Create a unique capture script such as:

`T:\Temp\RemoteControlMCP\capture-agy-status-<run-id>.ps1`

Example script:

```powershell
$raw = [Console]::In.ReadToEnd()

if (-not [string]::IsNullOrWhiteSpace($raw)) {
    $payload = $raw | ConvertFrom-Json -Depth 100

    $payload |
        Add-Member `
            -NotePropertyName captured_at `
            -NotePropertyValue ([DateTimeOffset]::Now.ToString('o')) `
            -Force

    $payload |
        ConvertTo-Json -Depth 100 -Compress |
        Add-Content `
            -LiteralPath 'T:\Temp\RemoteControlMCP\agy-status-<run-id>.jsonl' `
            -Encoding utf8
}

Write-Output 'AGY'
```

Temporarily add this settings block:

```json
{
  "statusLine": {
    "type": "command",
    "command": "pwsh.exe -NoProfile -File T:\\Temp\\RemoteControlMCP\\capture-agy-status-<run-id>.ps1"
  }
}
```

Preserve all existing settings while adding the temporary block.

### Baseline

Clear the unique capture file before launching the task.

During `agy --print`, choose the first payload for the new conversation that
contains:

- a non-empty matching `conversation_id`;
- a non-null model;
- a non-null quota object.

This is the pre-run quota baseline.

Retain the conversation ID.

### Final allowance snapshot

After `agy --print` exits, reopen that conversation without sending a prompt:

```powershell
& 'C:\Users\xman2\AppData\Local\agy\bin\agy.exe' `
    --conversation $conversationId `
    --log-file $inspectionLog
```

Allow it to initialise long enough for the statusline capture to receive an
updated payload, then stop the idle process.

Choose the latest captured payload with:

- the matching conversation ID;
- a non-null model;
- a non-null quota object;
- populated context usage.

Inspect the inspection log and verify that this no-prompt resume made no
request to:

- `streamGenerateContent`;
- `generateContent`.

Quota refresh calls are expected and do not constitute a model generation.

### Antigravity percentage calculations

Antigravity reports remaining fractions rather than used percentages.

For each active, non-disabled bucket:

```text
remaining before =
    before.remaining_fraction × 100

remaining after =
    after.remaining_fraction × 100

percentage points consumed =
    (before.remaining_fraction - after.remaining_fraction) × 100

used after =
    100 - remaining after
```

Report each relevant bucket independently.

Example:

```text
Antigravity allowance

Gemini five-hour quota:
- Before: 96.02% remaining
- After: 95.89% remaining
- Consumed: 0.13 percentage points
- Resets: 14 July 2026 at 12:49

Gemini weekly quota:
- Before: 19.16% remaining
- After: 19.14% remaining
- Consumed: 0.02 percentage points
- Resets: 17 July 2026 at 19:42

Plan: Google AI Pro
Model: Gemini 3.5 Flash (Medium)
```

Ignore disabled buckets when calculating consumption, but mention them if
their presence explains why a different provider quota was not used.

If another Antigravity process ran concurrently, label the observed change as
account-wide.

### Commands not to use for automated Antigravity usage inspection

Do not run:

```text
agy --print "/usage"
agy --print "/quota"
agy --print "/credits"
agy credits
agy usage
agy quota
```

Slash commands belong to the interactive TUI. These forms can become ordinary
model prompts or invalid subcommands.

Do not pipe `/usage` or `/credits` through ordinary redirected stdin. Without a
real interactive terminal, Antigravity can treat the text as a user prompt.

Use the statusline JSON mechanism instead.

### Secondary Antigravity token reporting

Antigravity persists exact per-generation usage in:

`C:\Users\xman2\.gemini\antigravity-cli\conversations\<conversation-id>.db`

Open the SQLite database read-only.

Read:

```sql
SELECT idx, data
FROM gen_metadata
ORDER BY idx;
```

For Antigravity 1.1.2 on Manta, each stored generation contains a protobuf
usage record with:

```text
Stored gen_metadata blob
└─ field 1: GenerationMetadata
   ├─ field 4: ModelUsageStats
   │  ├─ field 2: input tokens
   │  ├─ field 3: output tokens
   │  ├─ field 5: cache-read tokens
   │  ├─ field 9: thinking output tokens
   │  └─ field 10: visible response output tokens
   ├─ field 19: model alias
   └─ field 21: model display name
```

Sum all generation rows belonging to a new conversation.

For a continued conversation, capture the existing highest row index and
cumulative totals before the run, then report only new rows.

Validate every decoded generation:

```text
output_tokens ==
    thinking_output_tokens + visible_response_output_tokens
```

If this invariant fails, assume the internal format changed. Do not report
guessed values.

Version-check Antigravity before using the internal decoder. The statusline
quota feed is the primary supported measurement; database token decoding is
secondary.

Do not add cache-read tokens to input tokens.

Do not add thinking and visible-response output to output tokens because they
are subdivisions of it.

## Monitoring agent work

Do not infer that a task is active merely because `codex.exe`,
`codex app-server`, `node.exe`, `agy.exe` or an editor is running.

Background infrastructure can remain alive without an active coding turn.

For every launched run, retain:

- agent;
- model;
- process ID;
- start time;
- workspace;
- prompt path;
- stdout path;
- stderr path;
- agent-log path;
- event path;
- final-message path;
- conversation or thread ID;
- allowance baseline.

For Codex:

- `thread.started` identifies the thread;
- `turn.started` indicates work began;
- `item.started` and `item.completed` show operations;
- `turn.completed` indicates successful completion;
- `turn.failed` or `error` indicates failure.

A Codex app-server process alone proves nothing about turn status.

For Antigravity, use:

- process state;
- unique run log;
- captured conversation ID;
- stdout and stderr;
- conversation database modification time.

Do not launch a duplicate run simply because the first has produced no recent
stdout.

If a Git commit appears stuck, inspect for:

- repository hooks;
- commit signing;
- credential prompts;
- test or formatting hooks;
- child processes;
- lock files.

Establish what it is waiting for before terminating it.

## Verification

Do not rely solely on an agent's final message.

After implementation:

1. Inspect `git status --short --branch`.
2. Inspect the diff.
3. Confirm that only intended files changed.
4. Confirm the red test failed for the expected reason.
5. Confirm the green test passed.
6. Confirm broader validation results.
7. Check whether a commit was created when requested.
8. Inspect generated files or representative changed sections directly.
9. Report failed runs, retries and corrective passes.
10. Restore temporary configuration and remove helper files.
11. Report allowance percentage information before token information.

## Final report format

Use this structure:

```text
Agent: Codex or Antigravity
Model: <model>
Runs: <count and reason for retries>
Result: completed, failed, timed out or still running

Files changed:
<summary>

Red test:
<command and expected failure>

Green test:
<command and passing result>

Broader validation:
<results>

Commit:
<SHA or not committed>

Allowance:
<primary allowance window or quota bucket>
- Before: <percentage>
- After: <percentage>
- Consumed: <percentage-point change>
- Remaining: <percentage>
- Reset: <time>

<additional window or bucket where relevant>

Secondary token usage:
- Input: <tokens>
- Cached input/cache read: <tokens>
- Output: <tokens>
- Reasoning/thinking output: <tokens>
- Visible response output where available: <tokens>
- Total input plus output: <tokens>

Remaining concerns:
<none or details>
```

Never conceal a failed initial run by describing a later retry as the only
agent run.
