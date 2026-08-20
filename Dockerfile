# Built by `az acr build`, so the base images are pulled in Azure rather than here.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
# openssh-client: the server shells out to ssh-keygen to generate its host key on first start
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssh-client \
    && rm -rf /var/lib/apt/lists/*
# All three binaries. One image, three processes: the git server, the api server
# and the merge worker are built from the same source and deployed separately, so
# a shared image keeps them on the same commit without coupling their lifecycles.
# Each Deployment picks its process with `command`, so a binary missing here is a
# CrashLoopBackOff there, not a build error.
COPY --from=build /src/target/release/rustic-git /usr/local/bin/rustic-git
COPY --from=build /src/target/release/rustic-git-api /usr/local/bin/rustic-git-api
COPY --from=build /src/target/release/rustic-git-worker /usr/local/bin/rustic-git-worker
ENV RUSTIC_GIT_CACHE_DIR=/var/cache/rustic-git RUSTIC_GIT_HOST_KEY=/var/lib/rustic-git/host_key
EXPOSE 8080 2222
ENTRYPOINT ["rustic-git"]
CMD ["serve"]
