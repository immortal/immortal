FROM golang:latest as builder
RUN groupadd -r toor && useradd --create-home --no-log-init -r -g toor toor
RUN go get -u github.com/golang/dep/cmd/dep
WORKDIR /go/src/github.com/immortal/immortal
COPY . .
RUN dep ensure --vendor-only
RUN chown -R toor:toor /go
ARG VERSION=0.0.0
ENV VERSION="${VERSION}"
USER toor
RUN go test -race -v
USER root
RUN make build-linux
RUN mv man /
RUN mv build /

FROM ruby:2.3
RUN apt-get update && apt-get install -y --no-install-recommends -q build-essential ca-certificates git rpm
ARG VERSION=0.0.0
ENV VERSION="${VERSION}"
ENV GEM_HOME /usr/local/bundle
ENV BUNDLE_PATH="$GEM_HOME" \
    BUNDLE_BIN="$GEM_HOME/bin" \
    BUNDLE_SILENCE_ROOT_WARNING=1 \
    BUNDLE_APP_CONFIG="$GEM_HOME"
ENV PATH $BUNDLE_BIN:$PATH
RUN gem install --quiet --no-document fpm
RUN gem install --quiet --no-document package_cloud
RUN mkdir /build
RUN mkdir -p /source/etc/immortal \
  && mkdir -p /source/tmp/immortal \
  && mkdir -p /source/usr/local/man/man8
COPY --from=builder /build /build
COPY --from=builder /man/* /source/usr/local/man/man8/
RUN mkdir deb-package
WORKDIR deb-package
RUN git clone https://github.com/immortal/packages.git scripts
RUN cp scripts/* /source/tmp/immortal
RUN fpm --output-type deb \
  --input-type dir \
  --name immortal \
  --version ${VERSION} \
  --description 'A *nix cross-platform (OS agnostic) supervisor' \
  --url 'https://immortal.run' \
  --after-install scripts/after-install.sh \
  --before-remove scripts/before-remove.sh \
  --package immortal_${VERSION}_i386.deb \
  --architecture i386 \
  --chdir / \
  /source/=/ /build/386/=/usr/bin; done
RUN for arch in /build/*; do \
  fpm --output-type deb \
  --input-type dir \
  --name immortal \
  --version ${VERSION} \
  --description 'A *nix cross-platform (OS agnostic) supervisor' \
  --url 'https://immortal.run' \
  --after-install scripts/after-install.sh \
  --before-remove scripts/before-remove.sh \
  --package immortal_${VERSION}_${arch##*/}.deb \
  --architecture ${arch##*/} \
  --chdir / \
  /source/=/ /build/${arch##*/}/=/usr/bin; done
RUN for arch in /build/*; do \
  fpm --output-type rpm \
  --input-type dir \
  --name immortal \
  --version ${VERSION} \
  --description 'A *nix cross-platform (OS agnostic) supervisor' \
  --url 'https://immortal.run' \
  --after-install scripts/after-install.sh \
  --before-remove scripts/before-remove.sh \
  --package immortal_${VERSION}_${arch##*/}.rpm \
  --architecture ${arch##*/} \
  --chdir / \
  /source/=/ /build/${arch##*/}/=/usr/bin; done
