# ch-eval-base, the default image for eval tasks. `ch-eval build` builds it.
#
# Debian slim with git, ca-certificates, python3 and nodejs. `ch` is not in the image:
# each run mounts a static build at /usr/local/bin/ch, so the image doesn't change when
# ch does. A task that needs other tools names its own image in task.toml.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git nodejs python3 \
    && rm -rf /var/lib/apt/lists/*

# Runs use the host user's uid and gid, which have no account here and may not own the
# mounted /work as git sees it. Trust every repository, and give commits an author.
RUN git config --system --add safe.directory '*' \
    && git config --system user.name ch-eval \
    && git config --system user.email ch-eval@localhost \
    && git config --system init.defaultBranch main
