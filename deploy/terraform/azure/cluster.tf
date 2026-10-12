resource "random_password" "secret" {
  for_each = toset(["POSTGRES_PASSWORD", "CLICKHOUSE_PASSWORD", "ADMIN_TOKEN", "SANDBOX_ACCESS_TOKEN_HASH_SEED"])
  length   = 48
  special  = false
}

resource "random_bytes" "team_api_key" {
  length = 20
}

resource "kubernetes_namespace" "e2b" {
  metadata {
    name = "e2b"
  }
  lifecycle {
    ignore_changes = [metadata[0].annotations, metadata[0].labels]
  }
}

resource "kubernetes_secret" "e2b" {
  metadata {
    name      = "e2b-secrets"
    namespace = kubernetes_namespace.e2b.metadata[0].name
  }
  data = merge(
    { for key, password in random_password.secret : key => password.result },
    { TEAM_API_KEY = "e2b_${random_bytes.team_api_key.hex}" },
  )
}

locals {
  overlay = "${path.module}/../../k8s/overlays/${var.name}"
}

resource "local_file" "cluster_env" {
  filename = "${local.overlay}/cluster.env"
  content = join("\n", [
    "WILDCARD_HOSTNAME=*.${var.domain}",
    "API_HOSTNAME=api.${var.domain}",
    "DOMAIN_NAME=${var.domain}",
    "HUGEPAGES=${var.hugepages}",
    "HUGEPAGES_MEMORY=${var.hugepages * 2}Mi",
    "ROOTFS_EXPOSURE=${var.rootfs_exposure}",
    "TEMPLATE_STORAGE_URL=azblob://${azurerm_storage_container.templates.name}",
    "BUILD_CACHE_STORAGE_URL=azblob://${azurerm_storage_container.build_cache.name}",
    "AZURE_STORAGE_ACCOUNT_NAME=${azurerm_storage_account.this.name}",
    "AZURE_CLIENT_ID=${local.kubelet_identity.client_id}",
    "",
  ])
}

resource "local_file" "cluster_issuer" {
  filename = "${local.overlay}/cluster-issuer.yaml"
  content = yamlencode({
    apiVersion = "cert-manager.io/v1"
    kind       = "ClusterIssuer"
    metadata   = { name = "e2b" }
    spec = {
      acme = {
        email               = var.acme_email
        server              = "https://acme-v02.api.letsencrypt.org/directory"
        privateKeySecretRef = { name = "e2b-acme" }
        solvers = [{
          dns01 = {
            azureDNS = {
              subscriptionID    = var.subscription_id
              resourceGroupName = var.dns_zone_resource_group
              hostedZoneName    = var.dns_zone_name
              environment       = "AzurePublicCloud"
              managedIdentity   = { clientID = local.kubelet_identity.client_id }
            }
          }
        }]
      }
    }
  })
}

resource "local_file" "kustomization" {
  filename = "${local.overlay}/kustomization.yaml"
  content = yamlencode({
    apiVersion         = "kustomize.config.k8s.io/v1beta1"
    kind               = "Kustomization"
    namespace          = "e2b"
    resources          = ["../../base", "cluster-issuer.yaml"]
    components         = ["../../components/cluster"]
    configMapGenerator = [{ name = "e2b-cluster", envs = ["cluster.env"] }]
    images = [{
      name    = "E2B_ORCHESTRATOR_IMAGE"
      newName = split(":", local.orchestrator_image)[0]
      newTag  = split(":", local.orchestrator_image)[1]
    }]
    patches = [{
      target = { kind = "EnvoyProxy", name = "e2b" }
      patch = yamlencode([{
        op    = "add"
        path  = "/spec/provider/kubernetes/envoyService/annotations"
        value = { "service.beta.kubernetes.io/azure-pip-name" = azurerm_public_ip.gateway.name }
      }])
    }]
  })
}
