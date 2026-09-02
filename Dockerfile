FROM golang:1.26-alpine AS build
ARG VERSION=dev
ARG COMMIT=unknown
ARG BUILT_AT=unknown
WORKDIR /src
COPY go.mod go.sum ./
COPY cmd ./cmd
COPY internal ./internal
RUN CGO_ENABLED=0 go build -trimpath \
      -ldflags="-s -w -X easyacp/internal/buildinfo.Version=${VERSION} -X easyacp/internal/buildinfo.Commit=${COMMIT} -X easyacp/internal/buildinfo.BuiltAt=${BUILT_AT}" \
      -o /out/spin-server ./cmd/spin-server && \
    CGO_ENABLED=0 go build -trimpath \
      -ldflags="-s -w -X easyacp/internal/buildinfo.Version=${VERSION} -X easyacp/internal/buildinfo.Commit=${COMMIT} -X easyacp/internal/buildinfo.BuiltAt=${BUILT_AT}" \
      -o /out/spin-client ./cmd/spin-client

FROM alpine:3.24 AS server
RUN apk add --no-cache ca-certificates && mkdir -p /data
COPY --from=build /out/spin-server /usr/local/bin/spin-server
EXPOSE 8080
ENTRYPOINT ["spin-server"]
CMD ["-addr", ":8080", "-database", "/data/spin.db", "-state", "/data/spin-state.json"]

FROM alpine:3.24 AS client
RUN apk add --no-cache ca-certificates docker-cli && mkdir -p /client-data
COPY --from=build /out/spin-client /usr/local/bin/spin-client
ENTRYPOINT ["spin-client"]
