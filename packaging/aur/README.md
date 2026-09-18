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
   authenticate. Every host tried so far (controller, Mac, ts2, trj) is denied,
   and no AUR-specific identity is selected by any of their SSH configs. This
   needs an account name, host and key from the operator; `br` itself never
   performs this step.

2. **A `.SRCINFO` committed beside the `PKGBUILD`.** AUR's server-side hook
   rejects any push whose repository root lacks one, so credentials alone would
   not have been enough.

## `.SRCINFO` status

The `.SRCINFO` here was written by hand from `PKGBUILD`, because `makepkg` is
not available on this machine. It is expected to be correct, but it has **not**
been produced or verified by `makepkg`.

**Regenerate it on an Arch host before pushing**, and commit the result if it
differs:

```bash
cd packaging/aur
makepkg --printsrcinfo > .SRCINFO
git diff --exit-code .SRCINFO   # empty means the hand-written file was right
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
