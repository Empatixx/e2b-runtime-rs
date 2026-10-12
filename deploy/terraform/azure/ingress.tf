data "azurerm_dns_zone" "this" {
  name                = var.dns_zone_name
  resource_group_name = var.dns_zone_resource_group
}

locals {
  record_prefix = var.domain == var.dns_zone_name ? "" : ".${trimsuffix(var.domain, ".${var.dns_zone_name}")}"
}

resource "azurerm_public_ip" "gateway" {
  name                = "${var.name}-gateway"
  resource_group_name = azurerm_kubernetes_cluster.this.node_resource_group
  location            = azurerm_resource_group.this.location
  allocation_method   = "Static"
  sku                 = "Standard"
  zones               = ["1", "2", "3"]
  tags                = var.tags
}

resource "azurerm_dns_a_record" "wildcard" {
  name                = "*${local.record_prefix}"
  zone_name           = data.azurerm_dns_zone.this.name
  resource_group_name = var.dns_zone_resource_group
  ttl                 = 300
  records             = [azurerm_public_ip.gateway.ip_address]
}

resource "azurerm_role_assignment" "kubelet_dns" {
  scope                = data.azurerm_dns_zone.this.id
  role_definition_name = "DNS Zone Contributor"
  principal_id         = local.kubelet_identity.object_id
}

resource "helm_release" "envoy_gateway" {
  name             = "eg"
  repository       = "oci://docker.io/envoyproxy"
  chart            = "gateway-helm"
  version          = "v1.5.9"
  namespace        = "envoy-gateway-system"
  create_namespace = true
  wait             = true
  values = [yamlencode({
    deployment = { replicas = 2 }
  })]
}

resource "helm_release" "cert_manager" {
  name             = "cert-manager"
  repository       = "https://charts.jetstack.io"
  chart            = "cert-manager"
  version          = "v1.18.2"
  namespace        = "cert-manager"
  create_namespace = true
  wait             = true
  values = [yamlencode({
    crds   = { enabled = true }
    config = { apiVersion = "controller.config.cert-manager.org/v1alpha1", kind = "ControllerConfiguration", enableGatewayAPI = true }
  })]
  depends_on = [helm_release.envoy_gateway]
}
