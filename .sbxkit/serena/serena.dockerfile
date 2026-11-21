# syntax=docker/dockerfile:1
# Bundle Serena with a managed CPython via uv so the overlay does not depend
# on the workload's /usr/bin/python3. Install under /opt/serena and copy to
# scratch.
FROM dhi.io/debian-base:trixie-dev AS build
ARG SERENA_VERSION
ARG SERENA_PYTHON_VERSION=3.12
ARG TARGETARCH
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# Install uv into the build stage only; the final overlay keeps Serena's
# tool environment, not the uv installer itself (unless pulled in as a
# dependency of the tool tree).
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
ENV PATH=/root/.local/bin:$PATH

ENV UV_PYTHON_INSTALL_DIR=/out/opt/serena/python \
    UV_TOOL_DIR=/out/opt/serena/tools \
    UV_TOOL_BIN_DIR=/out/opt/serena/bin \
    UV_LINK_MODE=copy

RUN set -eux; \
    mkdir -p "$UV_PYTHON_INSTALL_DIR" "$UV_TOOL_DIR" "$UV_TOOL_BIN_DIR"; \
    uv tool install \
      --managed-python \
      --python "${SERENA_PYTHON_VERSION}" \
      "serena-agent==${SERENA_VERSION}"; \
    # Launcher must resolve inside the overlay tree.
    readlink -f /out/opt/serena/bin/serena | grep -q '^/out/opt/serena/'; \
    /out/opt/serena/bin/serena --help >/dev/null; \
    chown -R 0:0 /out; \
    # Serena may write project caches under the tool env; keep the tree
    # agent-writable where needed while bin/ stays executable.
    chown -R 1000:1000 /out/opt/serena

FROM scratch
COPY --from=build /out /
# On kit lifecycle install, this config is copied to $HOME/.serena
COPY serena_config.yml /opt/serena/share/serena_config.yml
ENV PATH=/opt/serena/bin \
    UV_PYTHON_INSTALL_DIR=/opt/serena/python \
    UV_TOOL_DIR=/opt/serena/tools \
    UV_TOOL_BIN_DIR=/opt/serena/bin
