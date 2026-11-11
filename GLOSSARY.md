# AgentLater

AgentLater makes HTTP requests later for agents and records whether they happened. Each project has its own ledger of tasks and the attempts made to send them.

## Projects

**Signing secret**:
What a project and its destinations share so that a destination can tell a request came from that project. Rotating it replaces it; the one replaced stays valid for an overlap the owner chooses, which can be none.
_Avoid_: signing key, webhook secret, API key, "secret" alone (that is a credential an agent stores for its requests)

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

**Made through A2A**:
Said of a request that arrived on the A2A interface. The ledger marks it, and A2A reads only marked tasks; every other interface reads all of a project's tasks and cannot tell which are marked.
_Avoid_: origin (issue #209 calls the mark an "origin marker"; here origin reads as the task kind), A2A task as a kind (its kind is request)

**Watch**:
A task whose attempts are polls, repeated until the response matches or the deadline passes.
_Avoid_: poller

**Action**:
The task a watch sends when it matches (its `on_match`).
_Avoid_: trigger, follow-up

**Callback**:
The task that posts another task's finish to its callback URL.

**Cron**:
A request an agent wants repeated on a schedule. Each slot of the schedule makes one cron run.
_Avoid_: job, schedule (the schedule is only its timing)

**Cron run**:
One task a cron makes for one slot of its schedule.

**Health**:
How a cron's runs have been going: pending until one finishes, healthy after a success, failing after a failed run or a run that could not start. Turning failing, and recovering, each send one alert.
_Avoid_: status (a task's status is something else)

**Alert**:
The task that posts a change in a cron's health to its alert URL.

**Reporting**:
Where a finished task's result goes (a callback URL, the inbox, or nowhere) and whether it carries the captured response. Separate from the task kind: many kinds can report.

## Inbox

**Inbox**:
Where events for the agent wait until it reads and acks them. Every project has one.
_Avoid_: queue, mailbox

**Event**:
One entry in the inbox: a task's report, a cron's alert, a hook's request, or a notice that the wake turned off. Numbered in the order it arrived.
_Avoid_: message, notification

**Ack**:
Marking every event up to a number as read. It only moves forward.

**Wake**:
The POST that tells an agent its inbox has unread events.
