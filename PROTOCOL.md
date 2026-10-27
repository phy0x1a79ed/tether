# The tether protocol

## Purpose & Contents

This file describes one tether session end to end: the invite code, the route
through the relay, the handshake, and the frames the two ends exchange.

What belongs here: the order of events across five crates and two hosts, which
no single source file shows, and the agreements that two separate files must
both honour. What does not belong here: anything one module already explains
better than a summary of it would. Each section names the file that owns its
subject. Read that file for the detail and read this one for the shape.

## Vocabulary

One set of words, used in the code, the verbs, the interface and these docs.

| Term | Means |
| --- | --- |
| **owner** | the person at the machine being helped, and the machine itself |
| **operator** | the trusted person invited in |
| **relay** | the public box the session passes through |
| **invite code** | three or more plain words, such as `acre anchor kettle`: the first names the slot, the rest are the phrase |
| **slot** | the session's address: a small number the relay issues, said as the code's first word |
| **phrase** | the rest of the code, which the relay never learns |
| **seat** | one of the two chairs at a slot, taken with a single-use token |
| **task** | one command or one terminal inside a session |
| **cut** | to end a session, from either side |
| **gap** | a dropped link that a consented session waits through, for up to 10 minutes |

## The five pieces

| Crate | Runs on | Does |
| --- | --- | --- |
| `tether-proto` | both ends | the invite code, the handshake, the frames |
| `tether-link` | both ends | dials the relay and runs the handshake |
| `tether-relay` | the public host | issues slots, pairs sockets, serves downloads |
| `tether-operator` | the operator's node | mints invites, drives sessions, answers a local socket |
| `tether-owner` | the owner's machine | asks consent, runs tasks, draws the screen |

`tether-link` exists so both ends dial the same way. Two implementations of one
handshake would drift, and a handshake that drifts fails as a wrong invite code
rather than as a version mismatch.

The operator's daemon keeps a journal of what each session did — every task, its
output, how it ended, and both sides of the conversation — numbered across the
daemon and stamped with an epoch it mints at startup. The epoch is what a reader
needs to know that a cursor from before a restart names nothing. Its `watch`
verb streams that journal and is the one request on the control socket that does
not answer and stop. Nothing in the journal carries the phrase: events are
addressed by slot, and the module that builds them will not name the types that
hold the secret.

## A session, in order

1. The operator's daemon posts to `/issue` with the bearer. The relay returns a
   **slot** and the operator's **seat token**.
2. The daemon mints a phrase locally. The phrase never leaves that process
   except to the person reading it out.
3. The daemon opens `/join/{slot}/{operator token}` and waits. Waiting costs
   the slot nothing.
4. The owner runs the launcher with the code — the shell one, or the PowerShell
   one at `/win`. The client posts to `/claim/{slot}` and gets the owner's
   single-use ticket.
5. The client opens `/join/{slot}/{ticket}`, or takes the chair by polling
   when its path refuses a WebSocket. The relay now has both chairs and starts
   moving bytes.
6. Both ends run the handshake. A wrong phrase fails here, at the peer.
7. The operator sends its `Hello`. The client shows the person who is asking,
   which relay, which code, and how to cut it.
8. The person answers. Only a yes sends the owner's `Hello`, so receiving that
   frame is how the operator learns consent was given. An owner with no
   terminal sends `Defer` instead and exits. The operator keeps the slot and
   waits at step 3 for the second run.
9. The daemon mints an owner seat token and a resume secret. It posts the
   token to `/hold/{slot}/{seat token}` and sends both to the client in `Hold`.
   The slot is now **held**, and a drop on either side is a gap rather than an
   end. See § Through a dropped link.
10. Tasks run. The owner watches each one as it happens.
11. Either side sends `Cut`. The other exits and says why.
12. The daemon posts to `/release/{slot}/{seat token}`. The slot stops existing,
    so the next person to run that code is turned away at once rather than
    seated opposite nobody.

The relay sees steps 1, 4, 5, 9, 12 and the bytes. It sees none of 6 through
11, and never the resume secret.

Step 12 has to be the operator's to take. The relay pumps sealed bytes and holds
no key, so a refusal and a mistyped phrase are the same event from where it
stands — and one must end the slot while the other must leave it open for the
retry. Only the daemon knows which happened. If it never says, an unheld slot
expires on its own a few seconds after its chair goes empty, and a held slot
after 10 minutes.

## Through a dropped link

Once the slot is held, both ends redial it for `RESUME_WINDOW`, 10 minutes,
from the moment each one saw the drop. The relay's teardown cascade still ends
both legs when one fails. The held slot is what makes the redial land.
`tether-link/src/resume.rs` owns the redial, the numbering and the resend.
`tether-relay/src/session.rs` owns the held slot.

A held slot changes four rules at the relay:

1. Either chair may stay empty for the window. A rejoining socket replaces a
   stale one in its chair, so a sleeping laptop's dead socket blocks nothing.
2. The owner's chair takes only the owner seat token, and takes it again on
   every redial.
3. `/claim` answers `404`, so no ticket can be minted for the slot.
4. The pairing budget stops counting.

Rules 3 and 4 hold because the phrase is dead after consent. A resumed channel
runs the same handshake with the resume secret as its password, under a
separate label, so it gets fresh keys and fresh counters and reuses no nonce. A
second run of the launcher, or a stranger who overheard the code, gets `404` and
"ask the operator for a new code". The budget protects a twenty-bit phrase, and
that phrase no longer opens anything.

Nothing sent is lost. Each end numbers the frames it sends, except `Ping`,
`Pong`, `Ack` and `Resume`, and keeps each one until the peer's `Ack` covers it.
A resumed channel starts with `Resume`, which says how many frames arrived, and
each end then resends the rest in order. The kept frames are bounded at 8 MiB.
When the bound is reached the owner stops reading a task's output, so the
command blocks on its own write instead of losing it.

An end notices a drop when its socket fails, when it hears nothing for a
minute, or when its wall clock jumped past its monotonic clock, which is how a
machine that slept looks from inside.

A deliberate end stays immediate. `Cut` goes in band when the link is up.
During a gap the leaving end posts `/release` with its own seat token instead.
The relay then keeps a tombstone, the slot number and its two seat tokens, for
the window, and answers the other end's next redial with `410`. A held slot
that merely expires leaves no tombstone, so each end's own window decides and
says why it ended.

**CAUTION** The resume secret and the owner seat token live in process memory
only. That is why nothing reconnects after a client exits and nothing starts at
boot, and it is also why a restart of the operator's daemon still ends every
session.

Resume runs only when both `Hello` frames say `resume`. An older peer never says
it, so it never receives `Hold`, `Ack` or `Resume`, which it could not decode,
and its session ends at the first drop as before.

## Why the slot and the phrase are separate

This is the design's load-bearing decision, and the tempting simplification
breaks it completely.

The tempting version derives the session id from the phrase, so the code is one
secret and the relay is told a hash of it. Two words carry twenty bits. A relay
holding that hash enumerates the whole space in milliseconds, recovers the
phrase, and then stands in the middle of the password-authenticated exchange —
which is the exact attack that exchange exists to stop. Salting, stretching and
hashing again change the cost by a constant and not the outcome.

So the two halves are secret in opposite ways. The relay issues the slot and
knows it, so the slot may travel in a URL and land in every access log on the
path. The relay never learns the phrase in plaintext, hashed, or in any derived
form. The slot exists precisely so the phrase never has to travel.

Twenty bits is defensible only because of what surrounds it. A session is
one-shot. It expires in minutes. It is destroyed after a few failed pairings.
Every guess must be a live attempt against a live session. Those three
properties are load-bearing, not hardening. They need to hold only until
consent, because a held slot no longer takes the phrase. `tether-proto/src/invite.rs` owns
this subject.

**CAUTION** Never put the phrase in a URL, a header, a query string or a log.
The public edge pins its access log level for this reason.

## Why only an operator may create a session

The owner's client redeems an invite. It cannot mint one. That inverts the
obvious arrangement, in which the machine needing help raises its hand, and the
inversion is the point: session creation would otherwise be an unauthenticated
public action on a public endpoint.

`/issue` and `/status` require the relay's bearer. Everything the owner touches
requires nothing. A stranger cannot bring a session into existence, so the
flood surface is closed rather than throttled, which is a different and much
better thing than a rate limit.

Everything the relay declines answers `404`, including a valid request carrying
a wrong bearer. A live slot, a spent slot, a slot that never existed and a
route that is not there are one answer, so the service cannot be mapped by the
shape of its refusals.

**Consequence, stated so nobody rediscovers it as a bug.** A machine cannot ask
for help on its own. Letting it do so is a separate decision with a different
threat model, and this design deliberately leaves no door open for it.

## The two grammars that must agree

The public edge refuses every path outside a fixed allow-list before it
consults any upstream. That is what turns "a slot the relay never issued" into
a refusal that happens before any pairing.

The grammar therefore lives in two places that must match exactly.

| Half | File | Owns |
| --- | --- | --- |
| the door | `awm/services/httpsfront/awm/httpsfront/tether.py` | which paths the edge forwards |
| the parser | `tether-proto/src/invite.rs`, `tether-relay/src/token.rs` | which slots and tokens the relay accepts |

A third name joins that family whenever a client is added. The launcher builds
the name it asks for, `artifacts.sh` names what is built, and the edge's
asset shape decides whether the name may be asked for at all. The Windows
launcher has a route of its own, `/win`, because PowerShell interprets what
comes back and `/bin` serves opaque bytes.

The polled routes are in the same family. The door carries `/poll/{slot}/{token}`
and `/seat/{id}/(recv|send|close)` because `tether-relay/src/poll.rs` serves
them. A door without them answers `404`, and an owner behind a proxy that
refuses WebSockets then has no transport left. The door carries
`/hold/{slot}/{token}` for the same reason. Without it the daemon's hold fails,
and every session runs without resume, silently.

A door looser than the parser forwards malformed input the door exists to
refuse. A door tighter than the parser turns a legitimate invite into a `404`
naming no cause, on the one path whose user has no account and no way to ask.
`test_tether_paths.py` writes both boundaries out, so a change on either side
has to change that file.

One more agreement, in the same family. The edge claims the `/tether` name on
every node, and whether a relay is configured decides `404` versus proxy. An
edge that classified the path only when a relay was wired would fall through to
the ordinary session check and answer `401`, which contradicts the verdict and
tells an anonymous caller that the path is special.

## When the path refuses a WebSocket

Some owners reach the internet only through an egress proxy. The client dials
through the proxy that `https_proxy` names, and `tether-link/src/net.rs` owns
that route. A proxy may still refuse the upgrade, or cut it. The client then
takes its chair with plain requests: one held open to receive and one to send.
`tether-relay/src/poll.rs` owns the routes, the numbering and the body format.

A polled chair is one more adapter onto the relay's pump, not a second pump. So
the idle deadline, the queue depth and the teardown cascade are the ones a
socket gets, and neither end of the sealed channel can tell how the other is
carried. Only the owner ever polls. The operator's daemon always uses the
socket, because it runs on a network its operator chose.

## Why the claim is a separate request

The owner's client claims a ticket on a plain `POST` before it upgrades. That
extra round trip looks removable and is not.

The edge forwards only `cookie`, `authorization` and `origin` across a
WebSocket upgrade, and no client-address header at all. The plain leg is the
last point on the path where the relay can still see who is calling, so the
per-address budget lives there or nowhere. The socket will not open without the
ticket the claim returns.

The seat token is not what keeps a session private. The handshake is, and would
be even if every token leaked. A token decides only who may take which chair.

## The handshake

Both ends hold the same weak phrase and derive a strong key from it, and an
eavesdropper who records the entire exchange gains nothing that enables offline
guessing. The relay records everything by definition, which is why an ordinary
key exchange with the phrase as a pre-shared key would look encrypted and
protect nothing.

A wrong phrase is caught at the **peer**, not at the relay. The relay has no
opinion on whether a phrase was right, because it never knew the phrase. The
owner sees "the other end has a different invite code" and may try again. The
relay counts the same event as a pairing, and a few of those destroy the slot.

`tether-proto/src/handshake.rs` owns the derivation, the transcript binding and
the per-direction keys. Read it there.

## The frames

Every frame rides inside the sealed channel, so the relay pumps bytes it cannot
parse. `tether-proto/src/frame.rs` is the definition and carries the reasoning
for each one. Four rules are worth stating where both ends can see them.

1. **Output precedes exit.** For a given task, every `Output` arrives before
   its `Exit`, and `Exit` is the last frame that task produces. A transcript
   showing an exit status above its own output would lie about what happened on
   the owner's machine.
2. **Consent precedes everything.** Until the owner's `Hello` arrives, the only
   frames the owner accepts are `Hello`, `Say` and `Cut`. An `Open` arriving
   early ends the session rather than being ignored.
3. **An empty `Input` is end of input. `Close` kills.** They were one frame
   once, and `cat` hung, which is how it was found.
4. **A `Resize` for task 0 is the owner's pane, not a task.** It is the owner
   saying how big a terminal they can see, and it travels one way only: the
   operator has no pane, so the owner refuses one carrying it.

Task 0 is a reserved name rather than a new frame, and the difference matters.
The operator mints task ids from one and always has, so the number named
nothing. An operator that does not know about it drops the frame like any other
it has no arm for and behaves exactly as it did before, which is what lets this
be added without a version bump. A new variant would instead be a decode failure
at every client already downloaded — and the owner's client is re-fetched every
session while the operator's daemon is the half that goes stale, so the cost
would land on exactly the wrong side.

Before this, the operator picked a size and the owner clipped whatever arrived,
silently. The owner's pane is now the authority: a grid they cannot see all of
breaks the sentence they consented to, and the operator is the end that can
afford to be letterboxed.

**CAUTION** `Defer` is the exception to that argument. It travels from the
owner to the operator, so the stale daemon is the end that has to decode it,
and an operator older than the frame drops the session instead of waiting for
the second run. Ship the operator and the clients together.

Frames are encoded with field names rather than positions. The two ends are
separate downloads and need not be the same build, so a field added mid-struct
must not be read as the field that used to sit there.

## Consent is a build property

The bypass used by the test harness is `--features test-consent-bypass` on
`tether-owner`. It is a build feature and not a runtime flag, because a flag
that exists eventually gets used, and an unattended connect path is the one
thing this tool refuses to have. In a shipped build that code does not exist to
be reached, and the launcher has nothing to plumb through.

The claim is tested rather than asserted, in both directions. One test reads a
release binary and checks the bypass marker's bytes are absent. A second builds
with the feature and checks the marker is present, because absence proves
nothing about a string that could never appear.

Two properties of the prompt are structural rather than careful. It reads from
the keyboard rather than standard input, because the launcher arrives through a
pipe and a prompt reading from that pipe would be answered by the operator. No
keyboard means not yet, because a question nobody heard has no answer.

That case is answered by a second run, not by a flag that skips the prompt. The
first run shows the whole prompt, then three words it generated locally, and
stops. The second run must carry those words and must find the record the first
left, under a per-user directory, for the same code, relay and operator. The
words are generated and shown on the owner's machine after the prompt and never
sent, so an operator cannot put a working confirmation in the line it reads
out. A first run that already carries one fails. `tether-owner/src/consent.rs`
and `tether-owner/src/confirm.rs` own this subject.

The keyboard has two spellings and one meaning. Unix opens `/dev/tty`, the
controlling terminal. Windows opens `CONIN$`, the console attached to this
process, which is a different handle from standard input and is not redirected
when standard input is. Both are the person, and neither is the pipe.

## What the relay keeps

No session content and no key material. State lives in memory. A restart
forgets every slot, and an invite not yet consented to ends with it. A held
session survives because its daemon puts the slot back: a bearer `/issue` that
names the slot and both seat tokens recreates it, and the owner's redial finds
it there. The only record of a finished session is a held slot's tombstone,
which lasts the window. The log records that a slot was issued, held and
released, and which leg of a pairing ended first and how. It says nothing else
about the session.
