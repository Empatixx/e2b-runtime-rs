resource "azurerm_container_registry" "this" {
  count               = var.orchestrator_image == "" ? 1 : 0
  name                = "${substr(replace(var.name, "-", ""), 0, 18)}${random_string.storage.result}"
  resource_group_name = azurerm_resource_group.this.name
  location            = azurerm_resource_group.this.location
  sku                 = "Standard"
  tags                = var.tags
}

resource "azurerm_role_assignment" "kubelet_pull" {
  count                = var.orchestrator_image == "" ? 1 : 0
  scope                = azurerm_container_registry.this[0].id
  role_definition_name = "AcrPull"
  principal_id         = local.kubelet_identity.object_id
}

resource "null_resource" "orchestrator_image" {
  count = var.orchestrator_image == "" ? 1 : 0
  triggers = {
    registry = azurerm_container_registry.this[0].name
    version  = var.orchestrator_version
  }
  provisioner "local-exec" {
    working_dir = "${path.module}/../../.."
    command     = "az acr build --registry ${azurerm_container_registry.this[0].name} --image orchestrator:${var.orchestrator_version} --file docker/orchestrator.Dockerfile --platform linux/amd64 --timeout 7200 ."
  }
}

locals {
  orchestrator_image = var.orchestrator_image != "" ? var.orchestrator_image : "${azurerm_container_registry.this[0].login_server}/orchestrator:${var.orchestrator_version}"
}
