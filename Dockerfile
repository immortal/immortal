FROM golang:latest

RUN groupadd -r toor && useradd --create-home --no-log-init -r -g toor toor
RUN go get -u github.com/golang/dep/cmd/dep
WORKDIR /go/src/github.com/immortal/immortal
COPY . .
RUN dep ensure --vendor-only
RUN chown -R toor:toor /go
USER toor

ENTRYPOINT ["go", "test", "-race", "-v"]
