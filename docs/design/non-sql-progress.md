# Non-SQL engines: decisions and progress

Plan: nosql-plan.md (same folder). User said (2026-11-21): "be autonomous, use your advisor for best plan possible and then execute to the end, shipping another release".

## Decisions (on top of the plan)
- S2 amendment accepted on the user's behalf (autonomy). Name it in PR 2 body, release notes, final report. SQL guard text/tests untouched.
- Where server behaviour is unconfirmed: choose the refusing option.
- Read-only: raw `db.runCommand` REFUSED in read-only mode (v1). Only typed ReadOp calls.
- Stage check: a stage document with != 1 key is refused.
- `server_read_only`: tri-state (Some(true)/Some(false)/None), fail closed: missing/empty privileges or failed probe => None (unknown), never true. Secondary counts only with directConnection.
- DynamoDB read-only: SELECT only, one statement, comments refused; EXISTS refused unless confirmed.
- PR 2 and PR 5 are minimal end-to-end (app arms, logo, form fields the compiler/tests need); PR 3 is polish.
- Independent adversarial review of PRs 2, 5, 6 (fresh agent, diff only: find input that passes read-only check and still writes).
- Release gate: real-app run of Mongo and DynamoDB under Xvfb. DocumentDB labelled untested against a real cluster.
- DocumentDB IAM deferred (aws-auth pulls aws-lc). Passwords + Secrets Manager only.
- Config version not bumped (#46 shipped in 0.13.0).

## Progress
- [x] D2 spike (branch spike/nosql-deps, 4d28ca3): mongodb 3.10.2 (rustls-tls, dns-resolver, bson-3, compat-3-3-0, no defaults) + aws-sdk-dynamodb 1.140.0 (rt-tokio, behavior-version-latest, no defaults): no aws-lc/openssl/native-tls/cmake; ring already present. Builds locally. gpui-kit 0.9.0 has feature tree-sitter-javascript. CI on 3 OSes: PASSED (run 44933756907). Delete branch spike/nosql-deps when done.
- [x] Local servers: no docker daemon by default; start with `(dockerd >log 2>&1 &)`. Images mongo:8.0 and amazon/dynamodb-local:latest pulled and run fine (ports 57017, 58000 used in probe).
- [x] PR 1 core refactor: #47 merged (cde7608). Note: integration test crates hit async "queries overflow the depth limit" when nesting another async fn under Db::run; try #![recursion_limit = "256"] in the test crate(s) first.
- [x] PR 2 Mongo + DocumentDB core: #48 merged (51dcc27). Adversarial review: no writes; fixed $_internalJs* JS bypass ($_ keys refused), maxTimeMS 60s cap, allowDiskUse refused; view-stored $function documented as limit. Disk: tb-redesign target hit 45 GB -> run `cargo clean` there between big PRs if df shows < 10 GB.
- [ ] PR 3 Mongo UI polish
- [ ] PR 4 DocumentDB AWS
- [ ] PR 5 DynamoDB (min e2e)
- [ ] PR 6 MCP
- [ ] PR 7 docs + release 0.14.0
