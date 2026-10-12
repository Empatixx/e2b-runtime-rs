output "kubeconfig_command" {
  description = "Writes the cluster credentials into ~/.kube/config."
  value       = "az aks get-credentials --resource-group ${azurerm_resource_group.this.name} --name ${azurerm_kubernetes_cluster.this.name}"
}

output "deploy_command" {
  description = "Deploys the stack into the cluster."
  value       = "kubectl apply --server-side -k deploy/k8s/overlays/${var.name}"
}

output "e2b_domain" {
  description = "E2B_DOMAIN for the SDK."
  value       = var.domain
}

output "e2b_api_key" {
  description = "E2B_API_KEY of the seeded team."
  value       = kubernetes_secret.e2b.data["TEAM_API_KEY"]
  sensitive   = true
}

output "gateway_ip" {
  description = "Public address of the Envoy gateway."
  value       = azurerm_public_ip.gateway.ip_address
}
