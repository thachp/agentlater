# AgentLater

AgentLater makes HTTP requests later for agents and records whether they happened. Each project has its own ledger of tasks and the attempts made to send them.

## Tasks

**Task**:
One HTTP request the ledger owns until it succeeds, fails, or is canceled, with every attempt to send it recorded.
_Avoid_: job, message

**Attempt**:
One try at sending a task. Every attempt is charged to the project's budget.
_Avoid_: "delivery" alone as a count ("delivery attempt" is fine), call

**Task kind**:
Who made a task: an agent's request or watch, a cron's run or alert, a wake, a watch's action, or another task's callback. Every task has exactly one, fixed when it is made.
_Avoid_: task type, origin

**Request**:
A task an agent asked for with schedule_http or queue_http, optionally waiting its turn in a lane.

**Watch**:
A task whose attempts are polls, repeated until the response matches or the deadline passes.
_Avoid_: poller

**Action**:
The task a watch sends when it matches (its `on_match`).
_Avoid_: trigger, follow-up

**Callback**:
The task that posts another task's finish to its callback URL.

**Cron run**:
One task a cron makes for one slot of its schedule.

**Alert**:
The task that posts a change in a cron's health to its alert URL.

**Wake**:
The POST that tells an agent its inbox has unread events.

**Reporting**:
Where a finished task's result goes (a callback URL, the inbox, or nowhere) and whether it carries the captured response. Separate from the task kind: many kinds can report.
