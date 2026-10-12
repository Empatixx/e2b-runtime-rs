resource "random_string" "storage" {
  length  = 6
  upper   = false
  special = false
}

resource "azurerm_storage_account" "this" {
  name                            = "${substr(replace(var.name, "-", ""), 0, 18)}${random_string.storage.result}"
  resource_group_name             = azurerm_resource_group.this.name
  location                        = azurerm_resource_group.this.location
  account_tier                    = "Standard"
  account_replication_type        = "ZRS"
  account_kind                    = "StorageV2"
  min_tls_version                 = "TLS1_2"
  allow_nested_items_to_be_public = false
  shared_access_key_enabled       = false
  tags                            = var.tags
}

resource "azurerm_storage_container" "templates" {
  name               = "templates"
  storage_account_id = azurerm_storage_account.this.id
}

resource "azurerm_storage_container" "build_cache" {
  name               = "build-cache"
  storage_account_id = azurerm_storage_account.this.id
}

resource "azurerm_role_assignment" "kubelet_blobs" {
  scope                = azurerm_storage_account.this.id
  role_definition_name = "Storage Blob Data Contributor"
  principal_id         = local.kubelet_identity.object_id
}
