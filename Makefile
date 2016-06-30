.PHONY: all get test clean build cover

GITHASH=$(shell git rev-parse HEAD)
GO ?= go
VERSION=0.0.1

all: clean build

get:
	${GO} get

build: get
#	${GO} get -u gopkg.in/yaml.v2;
	${GO} build -ldflags "-X main.version=${VERSION} -X main.githash=${GITHASH}" -o ir-scandir cmd/ir-scandir/main.go;
	${GO} build -ldflags "-X main.version=${VERSION} -X main.githash=${GITHASH}" -o immortal cmd/immortal/main.go;

clean:
	@rm -rf ir-* *.out immortalize build debian

test: get
	${GO} test -v

cover:
	${GO} test -cover && \
	${GO} test -coverprofile=coverage.out  && \
	${GO} tool cover -html=coverage.out
