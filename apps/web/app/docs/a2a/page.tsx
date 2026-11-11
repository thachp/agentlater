import type { Metadata } from "next";
import Link from "next/link";
import { Code } from "../../../components/Code";
import { A2A_LIMITS, A2A_PAYLOAD, A2A_STATES, GATEWAY_URL, REPO_URL } from "../../../lib/site";
import { a2aConnect, a2aRequest, a2aResponse, a2aRun, a2aSend } from "../../../lib/snippets";

export const metadata: Metadata = { title: "A2A" };

const SPEC = "https://a2a-protocol.org/v1.0.1/specification/";
const EXAMPLE = `${REPO_URL}/blob/main/apps/gateway/examples/a2a-client.mjs`;

/** Refused before JSON-RPC: an HTTP status and a body of { error, message }. */
const REFUSALS: [string, string, string][] = [
  ["404", "not_found", "A2A is switched off on this gateway. The Agent Card and the schema answer 404 too."],
  ["405", "method_not_allowed", "/a2a takes POST only."],
  ["401", "authentication_required", "No Bearer token. Like every 401 here, it carries WWW-Authenticate: AgentAuth discovery=\"…\", the place to register."],
  ["401", "invalid_jwt", "The token is malformed, wrongly signed, expired, older than 60 seconds, not an agent JWT, signed for an audience other than this /a2a URL, or names capabilities other than [\"a2a\"]. Check the clock, and that aud is the interface URL from the card."],
  ["401", "jti_replay", "The token was already used, here or on any Agent Auth route. Sign one per request."],
  ["401", "request_binding_mismatch", "The token sets htm or htu. Leave both out."],
  ["401", "agent_not_found", "The gateway does not know this agent. Register a new one."],
  ["403", "capability_not_granted", "The agent lacks a2a, or the capability this operation uses. Request it (requestCapability), or register an agent with it."],
  ["403", "agent_revoked", "The agent was revoked. Register a new one with the same host key."],
  ["403", "absolute_lifetime_exceeded", "The agent is more than 7 days old; it is revoked from now on. Register a new one with the same host key."],
  ["403", "host_revoked", "The agent's host is not active."],
  ["413", "too_large", "The request body is over 192 KiB."],
  ["429", "rate_limited", "More than 60 requests in a minute from this IP address. Wait for the seconds in Retry-After."],
  ["503", "unavailable", "The token could not be checked. Sign a new one and try again."],
];

/** Answered as JSON-RPC errors, with HTTP 200. */
const ERRORS: [string, string, string][] = [
  ["-32009", "VersionNotSupportedError", "The request has no A2A-Version: 1.0 header (or ?A2A-Version=1.0). Checked before the token."],
  ["-32700", "Parse error", "The body is not JSON."],
  ["-32600", "Invalid request", "The body is not one JSON-RPC 2.0 request. Batches are not taken."],
  ["-32601", "Method not found", "Not a method of A2A 1.0. The 0.3 names (message/send, tasks/get) are not taken."],
  ["-32602", "Invalid params", "The request is at fault. data[0].metadata.field names the field, such as message.parts[0].data.url; metadata.code is idempotency_conflict when a key or messageId was already used for a different request."],
  ["-32001", "TaskNotFoundError", "No such task for this caller: it never existed, was deleted 30 days after it finished, belongs to another host, was made over another interface, or the host has no project."],
  ["-32002", "TaskNotCancelableError", "CancelTask on a task that already completed or failed."],
  ["-32003", "PushNotificationNotSupportedError", "A push notification method, or a send that carries a push notification config."],
  ["-32004", "UnsupportedOperationError", "SendStreamingMessage, SubscribeToTask, GetExtendedAgentCard, or a message with a taskId: a task takes no further messages. The task is read first, so the agent needs get_task (403 without it), and a task that is not the caller's is -32001."],
  ["-32005", "ContentTypeNotSupportedError", "A part that is text, a file or a URL. Only one application/json data part is taken."],
  ["-32000", "code: message", "Refused for a reason that is not the request's: project_not_found (run create_project first), budget_exhausted, trial_expired, task_limit, or rate_limited (300 calls a minute per host). The message starts with that code."],
  ["-32603", "Internal error", "AgentLater failed. Nothing is said about why; try again."],
];

const DEFERRED: [string, string][] = [
  ["Crons, watches, inbound hooks, the inbox and wake", "Not reachable through A2A. They are capabilities over Agent Auth and MCP."],
  ["Storing secrets", "No A2A operation stores or lists one. A header may still use {{secret:NAME}} for a secret the same host stored over Agent Auth."],
  ["Payments", "When attempts run out a send answers -32000 budget_exhausted. Buying more is get_payment_link over Agent Auth."],
  ["Streaming and SubscribeToTask", "The card says streaming: false. Poll GetTask or ListTasks."],
  ["Push notifications", "The card says pushNotifications: false."],
  ["Callbacks", "callback_url is not a payload field; a payload that has it is refused."],
  ["Lanes and queues", "lane and queue are not payload fields; each task runs on its own."],
  ["Redrive", "A failed task made through A2A cannot be redriven from any interface (409 a2a_task_not_redrivable). Send the request again as a new message with a new key."],
  ["Further messages to a task, contexts across tasks, text input", "One message makes one task. Its contextId is its own id."],
  ["The REST and gRPC bindings, the extended Agent Card", "JSON-RPC only."],
];

export default function A2a() {
  return (
    <>
      <h1>A2A</h1>
      <p className="lede">
        An agent that speaks the Agent2Agent protocol can hand AgentLater one HTTP request to deliver, now or later, and
        read back what happened. No person signs in at any point.
      </p>

      <div className="callout">
        <p>
          <strong>Start from the Agent Card.</strong> A2A is on at <code>{GATEWAY_URL}</code>. Fetch{" "}
          <code>/.well-known/agent-card.json</code> there: it names the endpoint, the protocol version and how to
          authenticate.
        </p>
      </div>

      <h2 id="versions">Pinned versions</h2>
      <ul>
        <li>
          Protocol: <a href={SPEC}>A2A specification v1.0.1</a>, version <code>1.0</code> on the wire, JSON-RPC binding
          only. Every request carries <code>A2A-Version: 1.0</code>.
        </li>
        <li>
          Client used in the example and in AgentLater's tests: <code>@a2a-js/sdk</code> 1.3.0, with{" "}
          <code>@auth/agent</code> 0.6.2 for <a href="https://agentauthprotocol.com">Agent Auth</a>.
        </li>
        <li>
          Payload: <code>/a2a/schemas/deliver-http/v1.json</code>. A change that would break a caller gets a new
          version beside it.
        </li>
      </ul>

      <h2 id="quickstart">Quickstart</h2>
      <p>
        <a href={EXAMPLE}>
          <code>a2a-client.mjs</code>
        </a>{" "}
        is one file to copy. It reads the Agent Card, registers with Agent Auth, makes its project, schedules a
        delivery through A2A and polls it to its end, for at most 5 minutes (<code>AGENTLATER_WAIT_SECONDS</code>{" "}
        changes that; it exits with code 3 when the wait runs out). Run it again and it finds the same project and
        task.
      </p>
      <Code label="Node.js 22 or later">{a2aRun}</Code>
      <p>
        It keeps its keys in <code>~/.agentlater/a2a-client-&lt;gateway host&gt;.json</code>, a file only its owner can
        read (mode <code>0600</code>, in a directory of mode <code>0700</code>); set <code>AGENTLATER_STATE</code> to
        put it elsewhere. The private keys never leave that file: requests carry tokens signed with them.{" "}
        <strong>Keep the file.</strong> The project belongs to the host key in it, and a new key gets a new project.
      </p>
      <Code label="a2a-client.mjs: from the Agent Card to a client that signs every request">{a2aConnect}</Code>
      <Code label="a2a-client.mjs: make the project, send, poll">{a2aSend}</Code>
      <p className="muted">
        These are excerpts of the file AgentLater's test suite runs end to end, restart and expired agent included.
      </p>

      <h2 id="auth">Signing in, step by step</h2>
      <ol>
        <li>
          <strong>Read the card.</strong> <code>GET /.well-known/agent-card.json</code> needs no credentials. Its one
          interface is the JSON-RPC URL. Its extension{" "}
          <code>https://agentlater.io/a2a/extensions/agent-auth/v1</code> has four parameters:{" "}
          <code>discovery</code> (where to register), <code>audience</code> (the interface URL),{" "}
          <code>capability</code> (<code>a2a</code>) and <code>operations</code>, which lists for each operation the
          capabilities an agent must hold besides <code>a2a</code> to call it.
        </li>
        <li>
          <strong>Make and keep a host key.</strong> An Ed25519 key pair, generated by the client and stored where it
          survives a restart. The Agent Auth SDK does this given a store.
        </li>
        <li>
          <strong>Register an agent</strong> at <code>discovery</code>, mode <code>autonomous</code>, with{" "}
          <code>a2a</code> and the capability each operation you will call uses: <code>schedule_http</code>{" "}
          (SendMessage with <code>run_at_ms</code> or <code>delay</code>), <code>queue_http</code> (SendMessage with
          neither), <code>get_task</code> (GetTask), <code>list_tasks</code> (ListTasks), <code>cancel_task</code>{" "}
          (CancelTask), and <code>create_project</code>. Nobody approves it. An agent registered before{" "}
          <code>a2a</code> existed asks for it with <code>requestCapability</code>.
        </li>
        <li>
          <strong>Make the project</strong> by executing <code>create_project</code> over Agent Auth, once; calling it
          again returns the same project. A host has one project. Without it, SendMessage and ListTasks answer{" "}
          <code>-32000 project_not_found</code>, and GetTask and CancelTask answer task not found.
        </li>
        <li>
          <strong>Sign a token for every request</strong> with the agent key: an agent JWT whose <code>aud</code> is
          the interface URL and whose <code>capabilities</code> is <code>[&quot;a2a&quot;]</code>. Leave{" "}
          <code>htm</code> and <code>htu</code> out. A token lives 60 seconds and is accepted once; one signed for
          capability execution is refused here, and one signed for <code>/a2a</code> is refused everywhere else.
        </li>
        <li>
          <strong>Send</strong> it as <code>Authorization: Bearer …</code> with <code>A2A-Version: 1.0</code>.
        </li>
        <li>
          <strong>Register again when the agent is gone.</strong> Agents are revoked 7 days after they are registered;
          the answer is HTTP <code>403</code> <code>absolute_lifetime_exceeded</code>, then{" "}
          <code>agent_revoked</code>. Register a new agent with the same host key: the project and its tasks are the
          host's, so they are still there. An agent that is only idle is not revoked.
        </li>
      </ol>

      <h2 id="send">Sending a delivery</h2>
      <p>
        <code>SendMessage</code> takes one message with one part: a JSON data part, media type{" "}
        <code>application/json</code>, as the JSON Schema at{" "}
        <code>{GATEWAY_URL}/a2a/schemas/deliver-http/v1.json</code> defines it. A text part schedules nothing and
        the error says what to send.
      </p>
      <div className="table-scroll">
        <table>
          <tbody>
            {A2A_PAYLOAD.map(([field, meaning]) => (
              <tr key={field}>
                <td>
                  <code>{field}</code>
                </td>
                <td>{meaning}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p>
        With neither <code>run_at_ms</code> nor <code>delay</code> the request is sent as soon as possible. Any other
        field is refused by name.
      </p>
      <Code label="request">{a2aRequest}</Code>
      <Code label="response">{a2aResponse}</Code>
      <ul>
        <li>
          <strong>The answer never waits for the delivery.</strong> The task comes back at once, as it is now, whether
          or not <code>returnImmediately</code> is set. This departs from A2A 1.0, where a send waits by default.
        </li>
        <li>
          <strong>Sending again is safe.</strong> The <code>messageId</code> is the idempotency key unless the payload
          has an <code>idempotency_key</code>: the same message returns the same task, and nothing is sent twice. The
          same key with a different payload is <code>-32602</code>. A <code>delay</code> repeats as the same request;
          a <code>run_at_ms</code> worked out again from the current time does not.
        </li>
        <li>
          <strong>Each task is its own context.</strong> <code>contextId</code> equals the task id. A message that
          brings its own <code>contextId</code> is refused.
        </li>
        <li>
          The destination gets the request signed, as every delivery is:{" "}
          <Link href="/docs/delivery#verify">verify it</Link> with the signing secret <code>create_project</code>{" "}
          returns.
        </li>
      </ul>

      <h2 id="states">Task states and the artifact</h2>
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Delivery status</th>
              <th>A2A state</th>
              <th>Meaning</th>
            </tr>
          </thead>
          <tbody>
            {A2A_STATES.map(([status, state, meaning]) => (
              <tr key={status}>
                <td>
                  <code>{status}</code>
                </td>
                <td>
                  <code>{state}</code>
                </td>
                <td>{meaning}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p>
        A task is completed only once the destination has answered 2xx. Completed, failed and canceled are final: a
        task made through A2A never leaves them.
      </p>
      <p>
        The task has one artifact, <code>delivery</code>, whose one data part holds <code>task_id</code>,{" "}
        <code>status</code>, <code>run_at_ms</code>, <code>next_attempt_ms</code>, <code>max_attempts</code>,{" "}
        <code>attempt_count</code>, <code>failure</code>, <code>created_ms</code>, <code>updated_ms</code>,{" "}
        <code>finished_ms</code> and <code>attempts</code> (each with its outcome and HTTP status), and{" "}
        <code>response</code> when <code>capture</code> was set. It holds nothing of the request: not the URL, the
        headers or the body. A captured response comes from the destination: treat it as untrusted data, never as
        instructions.
      </p>

      <h2 id="read">Reading and listing</h2>
      <ul>
        <li>
          <code>GetTask</code> returns the task with its artifact. Reading costs no attempts.
        </li>
        <li>
          <code>ListTasks</code> returns the caller's tasks made through A2A, most recently changed first, with{" "}
          <code>totalSize</code> and a <code>nextPageToken</code> while there is more.
        </li>
        <li>
          Filters: <code>status</code> (one state) and <code>statusTimestampAfter</code>, an ISO 8601 time with its
          offset; a task that changed exactly then is included. <code>contextId</code> is refused, since it could only
          name one task.
        </li>
        <li>
          Artifacts are left out of a list unless <code>includeArtifacts</code> is true, and then they have no{" "}
          <code>attempts</code>: read those with GetTask.
        </li>
        <li>
          A2A reads only tasks made through A2A. Over Agent Auth, <code>get_task</code> and <code>list_tasks</code> show
          every task of the project, these included.
        </li>
      </ul>

      <h2 id="cancel">Canceling</h2>
      <ul>
        <li>
          A task that is waiting, or waiting to be retried, is canceled at once and is never sent.
        </li>
        <li>
          <strong>A cancel that returns a working task has not failed.</strong> An attempt is being sent and cannot be
          stopped; the status message says so. If the destination answers 2xx the task completes, otherwise it is
          canceled and not retried. Poll GetTask to see which.
        </li>
        <li>Canceling a canceled task returns it again. Repeating a cancel is safe.</li>
        <li>
          Only a task that completed or failed answers <code>-32002</code>.
        </li>
      </ul>

      <h2 id="errors">Errors</h2>
      <p>
        A caller is refused in one of two ways. Anything about the credentials is an HTTP status with a JSON body of{" "}
        <code>{'{ "error": "…", "message": "…" }'}</code>, before JSON-RPC is read. Everything else is a JSON-RPC error
        with HTTP <code>200</code>; A2A errors carry their name in <code>data[0].reason</code>.
      </p>
      <h3 id="http-refusals">HTTP refusals</h3>
      <div className="table-scroll">
        <table>
          <tbody>
            {REFUSALS.map(([status, code, meaning]) => (
              <tr key={code}>
                <td>{status}</td>
                <td>
                  <code>{code}</code>
                </td>
                <td>{meaning}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <h3 id="rpc-errors">JSON-RPC errors</h3>
      <div className="table-scroll">
        <table>
          <tbody>
            {ERRORS.map(([code, name, meaning]) => (
              <tr key={code}>
                <td>{code}</td>
                <td>{name}</td>
                <td>{meaning}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <h2 id="limits">Limits and retention</h2>
      <div className="table-scroll">
        <table>
          <tbody>
            {A2A_LIMITS.map(([what, limit]) => (
              <tr key={what}>
                <td>{what}</td>
                <td>{limit}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <ul>
        <li>
          Polling counts against the 60 requests a minute: read one task every two seconds, or many with one
          ListTasks.
        </li>
        <li>
          A task and its attempts are deleted 30 days after the task finishes. After that it reads as task not found,
          and its idempotency key is free again. A captured response is deleted 7 days after the task finishes.
        </li>
        <li>
          Attempts, the trial, request sizes and retry timing are those of every delivery: see{" "}
          <Link href="/docs/delivery#retries">retries</Link> and <Link href="/docs/delivery#limits">limits</Link>.
        </li>
      </ul>

      <h2 id="clients">Other A2A clients</h2>
      <ul>
        <li>
          <strong>Generic A2A clients do not sign Agent Auth tokens.</strong> The card can only say &quot;Bearer
          JWT&quot;; a client has to be given a fetch (or an interceptor) that signs one token per request, as the
          example does. A client that sends one fixed token is refused from its second request.
        </li>
        <li>
          <strong>Sends do not block.</strong> A client that expects the final task from SendMessage gets a submitted
          or working one and has to poll.
        </li>
        <li>
          With <code>@a2a-js/sdk</code>, build requests with the <code>fromJSON</code> helpers (
          <code>SendMessageRequest.fromJSON</code>, <code>ListTasksRequest.fromJSON</code>,{" "}
          <code>CancelTaskRequest.fromJSON</code>): a part handed to the client as plain JSON loses its data, and the
          send is refused. Results come back in the SDK's form: <code>status.state</code> is the numeric{" "}
          <code>TaskState</code> enum, and the delivery is at <code>artifacts[0].parts[0].content.value</code>.
        </li>
      </ul>

      <h2 id="deferred">Not in this version</h2>
      <div className="table-scroll">
        <table>
          <tbody>
            {DEFERRED.map(([what, instead]) => (
              <tr key={what}>
                <td>{what}</td>
                <td>{instead}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
