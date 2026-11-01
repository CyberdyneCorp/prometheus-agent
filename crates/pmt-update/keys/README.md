# Release signing keys

`primary.pub` and `standby.pub` are the minisign public keys compiled into
every `prometheus` binary (`pmt_update::TrustRoot::embedded`) and into the
rendered installers (`cargo xtask release installers`). Every release index,
manifest, archive and installer must carry a signature by one of them
(spec: distribution-and-update, Signature trust root).

The committed keys are placeholders whose secret halves were discarded: no
release can be signed until the maintainers replace them. To set up the real
keys:

1. `cargo xtask release keygen --out <offline dir> --name primary` and the
   same for `standby`; keep both `.key` files offline.
2. Commit the two `.pub` files here (and rebuild: every binary from then on
   trusts them).
3. Store the primary's `.key` text as the `PMT_RELEASE_SIGNING_KEY`
   repository secret; the Release workflow refuses to sign with any key that
   is not embedded (`--require-embedded`).

To rotate, sign with the standby key, then ship a release that embeds a new
standby. `test.pub` is trusted by debug builds only; its secret key is
committed at `../tests/fixtures/test.key` for tests.
