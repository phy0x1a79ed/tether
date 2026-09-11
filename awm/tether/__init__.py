"""tether — consent-gated remote assistance, in one vocabulary.

An **operator** invites themselves onto an **owner**'s machine by minting an
**invite code**: two plain words the owner types at their own keyboard. The
session crosses a **relay** neither side has to be reachable from, runs in front
of the owner, and either side can **cut** it.

This package is the gateway adapter and nothing else. Every byte of a session
is handled by the Rust binaries under ``rust/``.
"""
