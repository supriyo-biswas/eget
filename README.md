# eget

`eget` installs command-line tools distributed as standalone executables. Give
it a GitHub repository, a Gitea or GitLab URL, or a direct download URL, and it
will find a suitable release for your computer, extract archive files, and put
binaries in your `PATH`. Once installed, eget can automatically check these
tools for updates.

## Install eget

You can manually download `eget` like so:

```sh
curl -sSLfo eget \
  "https://github.com/supriyo-biswas/eget/releases/latest/download/eget-$(uname -sm | tr 'A-Z ' 'a-z-')"

chmod +x eget
mv eget ~/.local/bin # or any other location on your PATH
```

## Installing tools with `eget`

For GitHub, use the repository name. The `install` command is optional, and
you can install multiple tools at once:

```sh
eget BurntSushi/ripgrep eza-community/eza
# or alternatively,
eget install BurntSushi/ripgrep eza-community/eza
```

Specify a tag to download and pin a specific version, preventing any updates
from being downloaded for that tool.

```sh
eget install anomalyco/opencode:v1.18.3
```

By default, command symlinks are created in `~/.local/bin` for a normal user
or `/usr/local/bin` for root. Use `--to` to choose another directory for an
install, or set the `EGET_BIN_DIR` or `EGET_BIN` environment variable to change
the install default:

```sh
eget install --to "$HOME/bin" jgm/pandoc
EGET_BIN="$HOME/bin" eget jgm/pandoc
```

You can also pass URLs to other forges such as Gitlab and Gitea:

```sh
eget gitlab.com/gitlab-org/ci-cd/docker-machine
```

Tools hosted outside of these URLs are also supported. Simply pass in an URL
to a binary on an archive:

```sh
eget https://nodejs.org/dist/latest/node-v26.5.0-linux-x64.tar.xz
eget https://dl.min.io/aistor/mc/release/linux-amd64/mc
eget https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip
eget https://cache.agilebits.com/dist/1P/op2/pkg/v2.35.0/op_linux_amd64_v2.35.0.zip
```

For projects that produce versioned binaries only, you can specify an URL to
track version updates. For example:

```sh
eget install --version-url https://dl.k8s.io/release/stable.txt \
  "https://dl.k8s.io/release/{{version}}/bin/linux/amd64/kubectl"

eget install --version-url "https://go.dev/VERSION?m=text" \
  "https://go.dev/dl/{{version}}.linux-amd64.tar.gz"
```

## See what is installed

```sh
eget list
```

The list shows each tool's identifier, installed release, update status, and
command names. Results can be filtered by package ID prefix or owner.

## Update tools

Check all installed tools and choose whether to apply the available updates:

```sh
eget update
```

To check only particular tools, copy their package IDs from `eget list`:

```sh
eget update github.com/BurntSushi/ripgrep
```

Use `-y` to apply all available updates without prompting, or `--assume-no` to
check without applying them:

```sh
eget update -y
eget update --assume-no
```

Pinned tools are skipped during updates.

## Remove tools

Copy the tool identifier from `eget list`, then run:

```sh
eget uninstall github.com/BurntSushi/ripgrep
```

## Monorepo support

Some repositories publish multiple tools under the same project. `eget` will
try to infer if this is the case and will warn against it:

```console
$ eget install supriyo-biswas/static-builds
Error processing supriyo-biswas/static-builds: latest release tag gnu-sed-4.10
looks like a monorepo release; install a tool explicitly, for example:
eget install supriyo-biswas/static-builds:gnu-sed
```

You can then install one of the tools provided by that project, such as:

```sh
eget install supriyo-biswas/static-builds:gnu-sed \
  supriyo-biswas/static-builds:curl
```

In some cases, `eget` can automatically infer what you wanted to install even
when it is a monorepo repository, e.g.

```console
$ eget install kubernetes-sigs/kustomize
Installed github.com/kubernetes-sigs/kustomize:kustomize in project scope
```

## Authentication and private repositories

To work with private repositories, set a token in your environment before
installing or updating from a private repository:

```sh
export EGET_GITHUB_TOKEN="your-token"
export EGET_GITEA_TOKEN="your-token"
export EGET_GITLAB_TOKEN="your-token"
```

Self-hosted Gitea and GitLab instances use a token environment variable based
on their host name. If the domain name to your hosted repository ends in
`.com`, such as `gitlab.acmecorp.com`, the token should be stored in
`EGET_GITLAB_ACMECORP_TOKEN`, otherwise for non-`.com` domains, such as
`gitlab.acmecorp.io`, use `EGET_GITLAB_ACMECORP_IO_TOKEN`.

For direct download URLs, use the same host-derived environment variable name
and provide the complete `Authorization` header value, including its scheme.
For example, `EGET_DOWNLOADS_EXAMPLE_ORG_TOKEN="Bearer your-token"` is sent
verbatim when accessing `downloads.example.org`, including `--version-url`
checks.

## Using as a system level package manager

`eget` keeps installations separate by scope. `user` scope is the default for
regular users, however, root users have `system` scope by default. In `system`
scope, the binaries are installed in `/usr/local/bin` and the package
contents are stored in `/opt/eget`, so that they can be consumed by every
user.

This allows using `eget` as a system level package manager, simply by calling
`sudo eget`.

In rare cases, when operating as the root user, you may want to install a
package for use by the root user only. In these cases, use
`eget --scope=user ...` (or `EGET_SCOPE=user`), which will put the binaries in
`/root/.local/bin` and the package contents in `/root/.local/share/eget`.

## Using as a project package manager

`eget` also supports usage as a project-level package manager, which means you
can have per-project tooling without installing packages globally. In this
`project` scope of operation, `eget` will pin package dependencies
automatically so that people can use the same dependency versions when working
on the project.

Start by initializing a `eget-packages.txt` file in your project directory,
and then installing some tools, which you can execute using `eget x`
(or `eget exec`):

```console
$ > eget-packages.txt # create the file

$ eget getsops/sops
Installed github.com/getsops/sops in project scope (~/my-project)

$ eget x sops --version
sops 3.13.3 (latest)

$ cat eget-packages.txt # updated with new packages
github.com/getsops/sops:v3.13.3
```

When using a direct URL, you may need to substitute kernel and architecture
parameters since the project may be used on different OSes and architectures.
You can use MiniJinja expressions with conditional statements. For example, to
install go and node, you might use:

```console
eget install \
  "https://go.dev/dl/go1.26.0.{{kernel}}-{% if arch == 'x86_64' %}amd64{% else %}arm64{% endif %}.tar.gz"
  "https://nodejs.org/dist/latest/node-v26.5.0-{{kernel}}-{% if arch == 'x86_64' %}x64{% else %}arm64{% endif %}.tar.xz"
```

Then, you can use `eget x` as before:

```console
$ eget x go version
go version go1.26.5 linux/amd64

$ eget x node --version
v26.5.0
```

## Automated command activation

To avoid having to always prefix each command with `eget x`, you can use
[direnv](https://direnv.net/). Install per the
[instructions here](https://direnv.net/docs/installation.html) and configure
the `.envrc` project in your project directory, e.g:

```
cd my-project
echo 'export PATH="$PWD/.eget/bin:$PATH"' > .envrc
```

Then, run `direnv allow` to whitelist the `.envrc` to be executed
automatically when you `cd` into the directory. From now on, the project
scoped binaries will be activated automatically for shells inside inside it:

```console
$ cd my-project/
direnv: loading ~/my-project/.envrc
direnv: export ~PATH
$ which node
/home/me/my-project/.eget/bin/node
$ which go
/home/me/my-project/.eget/bin/go
```

## More information

Run `eget --help` or `eget <command> --help` for the complete command-line
reference.
