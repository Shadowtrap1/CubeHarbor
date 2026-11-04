# docker buildx bake [--push] [--set *.platform=linux/amd64,linux/arm64]
variable "REGISTRY" { default = "ghcr.io/shadowtrap1" }
variable "TAG" { default = "0.1.0" }

group "default" {
  targets = ["operator", "gateway", "dashboard", "server"]
}

target "_common" {
  context = "."
  dockerfile = "Dockerfile"
}

target "operator" {
  inherits = ["_common"]
  target = "operator"
  tags = ["${REGISTRY}/cubeharbor-operator:${TAG}"]
}

target "gateway" {
  inherits = ["_common"]
  target = "gateway"
  tags = ["${REGISTRY}/cubeharbor-gateway:${TAG}"]
}

target "dashboard" {
  inherits = ["_common"]
  target = "dashboard"
  tags = ["${REGISTRY}/cubeharbor-dashboard:${TAG}"]
}

# One server image per Java version; the operator picks the right one per Minecraft version.
target "server" {
  inherits = ["_common"]
  target = "server"
  name = "server-java${java}"
  matrix = { java = ["8", "17", "21", "25"] }
  args = { JAVA_VERSION = java }
  tags = ["${REGISTRY}/cubeharbor-server:${TAG}-java${java}"]
}
