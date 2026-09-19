# AUR packaging for `br`

`br-bin` is **not registered on AUR**. Verified against the live AUR RPC on
2026-09-18: `info?arg[]=br-bin` returns `resultcount 0`. The separately
maintained `beads-rust-bin` (0.2.7-1, maintainer `sQVe`, last modified
2026-05-13) is **not** this recipe and is not maintained by this project.

## Publishing checklist

AUR is the only publication venue still outstanding for v0.6.0
(`beads_rust-4e2n1`). GitHub, crates.io, Homebrew and Scoop are all published
and read back.

A push needs **both** of the following. Earlier sessions identified only the
first, which is why the publish never succeeded:

1. **An AUR account and registered SSH key.** `ssh aur@aur.archlinux.org` must
   authenticate. Every host tried so far (controller, Mac, ts2, trj) is denied
   with exactly `aur@aur.archlinux.org: Permission denied (publickey).`, and no
   AUR-specific identity is selected by any of their SSH configs. This needs an
   account name, host and key from the operator; `br` itself never performs this
   step.

2. **A `.SRCINFO` committed beside the `PKGBUILD`.** AUR's server-side hook
   rejects any push whose repository root lacks one, so credentials alone would
   not have been enough.

## `.SRCINFO` status — validated against a `makepkg` run

This file was written by hand from `PKGBUILD` (no `makepkg` on the authoring
machine), then checked against a `.SRCINFO` produced during the 2026-09-12
Arch packaging session and found **byte-identical**.

That session really did run `makepkg` on this exact recipe — its log opens with
`==> Making package: br-bin 0.6.0-1` and reports
`Validating source_x86_64 files with sha256sums... Passed` — so the comparison
is against a genuine Arch-environment artifact, not another hand transcription.

The evidence lived only in `/tmp` and was never committed, which is why it was
described as "retained" on `beads_rust-phm7n` while no `.SRCINFO` existed in
this repository. It is now preserved at
`/data/tmp/br-4e2n1-aur-evidence-20260918/` with `SHA256SUMS.txt`.

Regenerating before a push is still the cheap confirmation, since `PKGBUILD`
may have moved on:

```bash
cd packaging/aur
makepkg --printsrcinfo > .SRCINFO
git diff --exit-code .SRCINFO   # empty means this file is still correct
```

## Source checksums

The two `sha256sums` in `PKGBUILD` were verified on 2026-09-18 against the
published `.sha256` sidecars of the v0.6.0 GitHub release and match exactly:

| target | archive | sha256 |
|---|---|---|
| `x86_64` | `br-0.6.0-linux_amd64.tar.gz` | `f6f9a1663bae31e94d2dcfec62163f15b17f822711c486e860183a654784829b` |
| `aarch64` | `br-0.6.0-linux_arm64.tar.gz` | `dd865e02f05a5efa84aca25e70c4f054b358cb71e42580af97cf41fda52b46f5` |

Re-verify after any release bump:

```bash
for a in linux_amd64 linux_arm64; do
  curl -fsSL "https://github.com/Dicklesworthstone/beads_rust/releases/download/v${VERSION}/br-${VERSION}-${a}.tar.gz.sha256"
done
```

## What is not automated

`br` never pushes to AUR, and nothing in this repository does. The push is a
deliberate operator action against an external public registry. `PKGBUILD-git`
is the VCS variant and is not part of the v0.6.0 binary publication.
