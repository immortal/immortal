.PHONY: all get test clean build cover compile goxc bintray install uninstall docker linux

GO ?= go
GO_XC = ${GOPATH}/bin/goxc -os="freebsd netbsd openbsd darwin linux"
GOXC_FILE = .goxc.json
GOXC_FILE_LOCAL = .goxc.local.json
VERSION=$(shell git describe --tags --always)
DESTDIR ?= /usr/local

all: clean build

get:
	${GO} get
	${GO} get -u github.com/nbari/violetear;
	${GO} get -u github.com/immortal/logrotate;
	${GO} get -u github.com/immortal/multiwriter;
	${GO} get -u github.com/immortal/natcasesort;
	${GO} get -u github.com/immortal/xtime;

build: get
	${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o immortal cmd/immortal/main.go;
	${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o immortalctl cmd/immortalctl/main.go;
	${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o immortaldir cmd/immortaldir/main.go;

build-linux:
	for arch in 386 amd64 arm arm64 ppc64 ppc64le mips mipsle mips64 mips64le; do \
		mkdir -p build/$${arch}; \
		GOOS=linux GOARCH=$${arch} ${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o build/$${arch}/immortal cmd/immortal/main.go; \
		GOOS=linux GOARCH=$${arch} ${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o build/$${arch}/immortalctl cmd/immortalctl/main.go; \
		GOOS=linux GOARCH=$${arch} ${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o build/$${arch}/immortaldir cmd/immortaldir/main.go; \
	done

build-fbsd:
	mkdir -p build/amd64; \
	GOOS=freebsd GOARCH=amd64 ${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o build/amd64/immortal cmd/immortal/main.go; \
	GOOS=freebsd GOARCH=amd64 ${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o build/amd64/immortalctl cmd/immortalctl/main.go; \
	GOOS=freebsd GOARCH=amd64 ${GO} build -ldflags "-s -w -X main.version=${VERSION}" -o build/amd64/immortaldir cmd/immortaldir/main.go; \

clean:
	${GO} clean -i
	@rm -rf immortal immortalctl immortaldir *.debug *.out build debian

test: get
	${GO} test -race -v

cover:
	${GO} test -cover && \
	${GO} test -coverprofile=coverage.out && \
	${GO} tool cover -html=coverage.out

compile: clean goxc

goxc:
	$(shell echo '{\n  "ConfigVersion": "0.9",' > $(GOXC_FILE))
	$(shell echo '  "AppName": "immortal",' >> $(GOXC_FILE))
	$(shell echo '  "ArtifactsDest": "build",' >> $(GOXC_FILE))
	$(shell echo '  "PackageVersion": "${VERSION}",' >> $(GOXC_FILE))
	$(shell echo '  "TaskSettings": {' >> $(GOXC_FILE))
	$(shell echo '    "bintray": {' >> $(GOXC_FILE))
	$(shell echo '      "downloadspage": "bintray.md",' >> $(GOXC_FILE))
	$(shell echo '      "package": "immortal",' >> $(GOXC_FILE))
	$(shell echo '      "repository": "immortal",' >> $(GOXC_FILE))
	$(shell echo '      "subject": "nbari"' >> $(GOXC_FILE))
	$(shell echo '    }\n  },' >> $(GOXC_FILE))
	$(shell echo '  "BuildSettings": {' >> $(GOXC_FILE))
	$(shell echo '    "LdFlags": "-s -w -X main.version=${VERSION}"' >> $(GOXC_FILE))
	$(shell echo '  }\n}' >> $(GOXC_FILE))
	$(shell echo '{\n "ConfigVersion": "0.9",' > $(GOXC_FILE_LOCAL))
	$(shell echo ' "TaskSettings": {' >> $(GOXC_FILE_LOCAL))
	$(shell echo '  "bintray": {\n   "apikey": "$(BINTRAY_APIKEY)"' >> $(GOXC_FILE_LOCAL))
	$(shell echo '  }\n } \n}' >> $(GOXC_FILE_LOCAL))
	${GO_XC}

bintray:
	${GO_XC} bintray

install: all
	install -d ${DESTDIR}/bin
	install -d ${DESTDIR}/share/man/man8
	install immortal ${DESTDIR}/bin
	install immortalctl ${DESTDIR}/bin
	install immortaldir ${DESTDIR}/bin
	cp -R man/*.8 ${DESTDIR}/share/man/man8

uninstall: clean
	@rm -f ${DESTDIR}/bin/immortal
	@rm -f ${DESTDIR}/bin/immortalctl
	@rm -f ${DESTDIR}/bin/immortaldir
	@rm -f ${DESTDIR}/share/man/man8/immortal*

docker:
	docker build -t immortal --build-arg VERSION=${VERSION} .

docker-no-cache:
	docker build --no-cache -t immortal --build-arg VERSION=${VERSION} .

linux:
	docker run --entrypoint "/bin/bash" -it --privileged immortal
