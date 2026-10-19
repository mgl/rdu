FROM scratch

ARG TARGETARCH
COPY --chmod=755 ${TARGETARCH}/rdu /usr/bin/rdu
COPY LICENSE /usr/share/licenses/rdu/LICENSE

WORKDIR /data
ENTRYPOINT ["/usr/bin/rdu"]