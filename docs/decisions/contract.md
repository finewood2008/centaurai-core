# First-class Decisions contract

Decisions are server-owned resources. Clients create a draft, start or resume it, and observe the same persisted state from any authenticated device. A client never executes a Brain, assembles a prompt from retrieval hits, or supplies a trusted citation.

## Domain layers

- **Brain** is a provider/model or ACP-agent identity. Provider credentials remain inside Core.
- **Role** supplies the expert perspective and instructions independently of the model.
- **Tool** names an allowed capability. Tool configuration is persisted with the decision, while secrets stay in the owning service.

Core selects three Brains by default and accepts at most seven. Automatic selection takes the first enabled model from different providers before taking additional models from a provider. Timeout, rate-limit, and transient provider failures are retried once; a different provider is then attempted as a fallback. Successful opinions survive other Brain failures.

## State and recovery

`draft → running → completed|failed` is the normal path. A running session may become `paused` or `cancelled`. An owner interjection pauses an active run before the new turn is stored; `continue` creates a new session revision and only runs Brains without a completed opinion. A partial completed resolution can also continue to recover its failed Brains.

On process startup, interrupted `running` decisions and sessions become `paused`; running Brains return to `pending`. REST reads then expose enough state for another device to continue the decision.

## Knowledge boundary

The decision crate depends on `DecisionKnowledgePort`, not on a worker implementation. Create, start, and refresh invoke that port with the authenticated user, question, and persisted knowledge policy. The application must compose the Core Knowledge Gateway adapter through `DecisionService::new_with_knowledge`.

The default adapter returns no hits and never fabricates citations. Evidence submitted in a public request is rewritten as a `user_note`, receives a new server ID, score zero, and no document locator. Only results returned by `DecisionKnowledgePort` retain source IDs and locators. Retrieved snippets are included in cloud prompts only when the persisted policy explicitly sets `cloud_use: true`.

## Events

Events are user-scoped and use the following camelCase names:

1. `decision.sessionChanged`
2. zero or more `decision.turnDelta`
3. zero or more `decision.evidenceAdded`
4. `decision.completed`

Provider failure never terminates the shared event stream. `decision.completed` is emitted after all runnable Brains have reached a persisted outcome and at least one valid candidate exists.
