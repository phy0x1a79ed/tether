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
| **invite code** | a slot and two or more plain words, such as `7 anchor kettle` |
| **slot** | the code's first token, a small number the relay issues |
| **phrase** | the rest of the code, which the relay never learns |
| **seat** | one of the two chairs at a slot, taken with a single-use token |
| **task** | one command or one terminal inside a session |
| **cut** | to end a session, from either side |

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

## A session, in order

1. The operator's daemon posts to `/issue` with the bearer. The relay returns a
   **slot** and the operator's **seat token**.
2. The daemon mints a phrase locally. The phrase never leaves that process
   except to the person reading it out.
3. The daemon opens `/join/{slot}/{operator token}` and waits. Waiting costs
   the slot nothing.
4. The owner runs the launcher with the code. The client posts to
   `/claim/{slot}` and gets the owner's single-use ticket.
5. The client opens `/join/{slot}/{ticket}`. The relay now has both chairs and
   starts moving bytes.
6. Both ends run the handshake. A wrong phrase fails here, at the peer.
7. The operator sends its `Hello`. The client shows the person who is asking,
   which relay, which code, and how to cut it.
8. The person answers. Only a yes sends the owner's `Hello`, so receiving that
   frame is how the operator learns consent was given.
9. Tasks run. The owner watches each one as it happens.
10. Either side sends `Cut`. The other exits and says why.

The relay sees steps 1, 4, 5 and the bytes. It sees none of 6 through 10.

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
properties are load-bearing, not hardening. `tether-proto/src/invite.rs` owns
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
for each one. Three rules are worth stating where both ends can see them.

1. **Output precedes exit.** For a given task, every `Output` arrives before
   its `Exit`, and `Exit` is the last frame that task produces. A transcript
   showing an exit status above its own output would lie about what happened on
   the owner's machine.
2. **Consent precedes everything.** Until the owner's `Hello` arrives, the only
   frames the owner accepts are `Hello`, `Say` and `Cut`. An `Open` arriving
   early ends the session rather than being ignored.
3. **An empty `Input` is end of input. `Close` kills.** They were one frame
   once, and `cat` hung, which is how it was found.

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
`/dev/tty` rather than standard input, because the launcher arrives through a
pipe and a prompt reading from that pipe would be answered by the operator. No
terminal means no, because the answer to a question nobody heard is no.
`tether-owner/src/consent.rs` owns this subject.

## What the relay keeps

Nothing. No session content, no key material, no record of a finished session.
State lives in memory, so restarting the relay ends every live session. Its log
records that a slot was issued and says nothing else about the session.
