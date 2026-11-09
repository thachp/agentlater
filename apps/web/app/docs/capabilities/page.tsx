import { capabilities } from "@runlater/contracts/capabilities";
import type { Metadata } from "next";

export const metadata: Metadata = { title: "Capabilities" };

type Prop = {
  type?: string;
  enum?: string[];
  description?: string;
  default?: unknown;
  maximum?: number;
  minimum?: number;
  additionalProperties?: unknown;
};

function typeOf(p: Prop): string {
  if (p.enum) return p.enum.join(" | ");
  if (p.type === "object" && typeof p.additionalProperties === "object") return "object (string values)";
  return p.type ?? "any";
}

function details(p: Prop): string {
  const parts = [p.description ?? ""];
  if (p.minimum !== undefined && p.maximum !== undefined) parts.push(`${p.minimum}–${p.maximum}.`);
  if (p.default !== undefined) parts.push(`Default ${String(p.default)}.`);
  return parts.filter(Boolean).join(" ");
}

const ERRORS: [string, number, string][] = [
  ["invalid_request", 400, "An argument is wrong; field names it."],
  ["project_not_found", 404, "Call create_project first."],
  ["task_not_found", 404, "No such task in this project."],
  ["budget_exhausted", 402, "Not enough delivery attempts left; get_payment_link buys more."],
  ["trial_expired", 403, "The trial or bought attempts have expired; get_payment_link buys more."],
  ["billing_unavailable", 503, "Buying attempts is not available right now."],
  ["host_revoked", 403, "This agent's identity was revoked."],
  ["idempotency_conflict", 409, "The idempotency_key was used for a different request."],
  ["task_finished", 409, "Can't cancel: the task already succeeded or failed."],
  ["task_not_failed", 409, "Only failed tasks can be redriven."],
  ["rate_limited", 429, "More than 300 calls in a minute from one agent host or connection; wait a minute."],
  ["task_limit", 429, "The project holds 1,000 tasks (20,000 once paid); finished ones are purged after 30 days."],
];

export default function Capabilities() {
  return (
    <>
      <h1>Capabilities</h1>
      <p className="lede">
        Everything an agent can do. Each is an Agent Auth capability, executed by name with JSON arguments. This page
        is generated from the same definitions the gateway serves.
      </p>
      <p>
        Results are the task or project as JSON. Tasks never echo header values or the body back, since they may hold
        credentials.
      </p>

      {capabilities.map((c) => {
        const props = Object.entries(c.input.properties) as [string, Prop][];
        const required = new Set(c.input.required ?? []);
        return (
          <section className="capability" id={c.name} key={c.name}>
            <h2>{c.name}</h2>
            <p>{c.description}</p>
            {props.length === 0 ? (
              <p className="muted">No arguments.</p>
            ) : (
              <div className="table-scroll">
                <table>
                  <thead>
                    <tr>
                      <th>Argument</th>
                      <th>Type</th>
                      <th>Details</th>
                    </tr>
                  </thead>
                  <tbody>
                    {props.map(([name, p]) => (
                      <tr key={name}>
                        <td>
                          <code>{name}</code>
                          {required.has(name) ? <span className="req">required</span> : null}
                        </td>
                        <td>{typeOf(p)}</td>
                        <td>{details(p)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>
        );
      })}

      <h2 id="errors">Errors</h2>
      <p>
        A failed execution returns an error code and a message; validation errors also name the <code>field</code>.
      </p>
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Code</th>
              <th>Status</th>
              <th>Meaning</th>
            </tr>
          </thead>
          <tbody>
            {ERRORS.map(([code, status, meaning]) => (
              <tr key={code}>
                <td>
                  <code>{code}</code>
                </td>
                <td>{status}</td>
                <td>{meaning}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
