# Why the bridge is a functor

The mediation map in `ccn-core` is one table, and it is worth saying what kind of object it is,
because the shape is what makes the security property checkable rather than merely asserted.

## The near-isomorphism nobody planned

Claude Code's built-in tools and nucleus's pod-local tool-proxy routes were designed years apart by
people solving different problems. They line up almost exactly:

| Claude Code built-in | `nucleus-tool-proxy` route |
|---|---|
| `Bash` | `POST /v1/run` |
| `Read` | `POST /v1/read` |
| `Write`, `Edit`, `NotebookEdit` | `POST /v1/write` |
| `Glob` | `POST /v1/glob` |
| `Grep` | `POST /v1/grep` |
| `WebFetch` | `POST /v1/web_fetch` |
| `WebSearch` | `POST /v1/web_search` |
| `Agent`, `Task` | `POST /v1/pod/create` |

That is not a coincidence so much as convergent design: both are enumerations of *the effects a
coding agent can have on the world*, and there are not many. Nucleus's own
`DISALLOWED_BUILTIN_TOOLS` constant lists the same set from the other direction.

The consequence is that the bridge is not an adapter with special cases. It is a **map between two
alphabets of effects**, and the interesting content is the two places it is not a bijection:

- **Three-to-one on the left.** `Write`, `Edit` and `NotebookEdit` all land on `/v1/write`. The
  collapse is forced: the pod owns the file, so an edit computed on the host would be a write the
  lattice never inspected. The cost is real (`Edit` becomes read-then-write) and it is the correct
  trade.
- **Not total on the right.** `TodoWrite`, `BashOutput` and `KillShell` have no image. Rather than
  inventing routes, the map sends them to `Denied` *with a reason*, which keeps the gap legible
  instead of letting it read as an oversight.

## Totality is the security property

A partial map here is a hole. So `disposition : &str → Disposition` is total, with `Disposition`
closed over exactly `{Mediated, Denied}`. Two things follow that a test could not give you:

1. **Adding a tool name without a disposition does not compile.** The property is discharged where
   the mistake would be made, not in CI afterwards.
2. **The fall-through arm denies.** Totality over a *closed* list would still leave the open world
   unhandled; `unknown_tool_disposition` closes it, which is why a Claude Code release that ships a
   new built-in cannot silently widen the boundary.

This is the cheapest tier of check there is: it is decided from the source alone, needs no pod, no
network and no runner, and it fails at the moment of authorship. Every property that *can* be
pushed to that tier should be, because the alternative is discovering a mediation gap from a
receipt that does not exist.

## The one law that needs a test

Totality does not imply the two halves agree. The gate telling the model to call
`mcp__nucleus__run` while the server advertises no `run` tool is not a denial — it is a **deadlock**,
and it type-checks perfectly. So `mediated_tools()` derives the served list *from the same map* the
gate reads, and `every_mediated_target_is_actually_served` asserts the round trip. Deriving rather
than duplicating is what keeps it true; the test is there because "derived" is a claim about code
that can be edited.
