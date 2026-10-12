resource "azurerm_resource_group" "this" {
  name     = var.name
  location = var.location
  tags     = var.tags
}

resource "azurerm_virtual_network" "this" {
  name                = var.name
  resource_group_name = azurerm_resource_group.this.name
  location            = azurerm_resource_group.this.location
  address_space       = ["10.240.0.0/16"]
  tags                = var.tags
}

resource "azurerm_subnet" "nodes" {
  name                 = "nodes"
  resource_group_name  = azurerm_resource_group.this.name
  virtual_network_name = azurerm_virtual_network.this.name
  address_prefixes     = ["10.240.0.0/20"]
}

resource "azurerm_kubernetes_cluster" "this" {
  name                = var.name
  resource_group_name = azurerm_resource_group.this.name
  location            = azurerm_resource_group.this.location
  dns_prefix          = var.name
  kubernetes_version  = var.kubernetes_version
  node_resource_group = "${var.name}-nodes"
  tags                = var.tags

  default_node_pool {
    name                        = "control"
    vm_size                     = var.control_vm_size
    os_sku                      = "Ubuntu"
    vnet_subnet_id              = azurerm_subnet.nodes.id
    auto_scaling_enabled        = true
    min_count                   = var.control_min_count
    max_count                   = var.control_max_count
    node_labels                 = { "e2b.dev/role" = "control" }
    temporary_name_for_rotation = "controltmp"
    upgrade_settings {
      max_surge = "33%"
    }
  }

  identity {
    type = "SystemAssigned"
  }

  network_profile {
    network_plugin      = "azure"
    network_plugin_mode = "overlay"
    load_balancer_sku   = "standard"
    pod_cidr            = "10.244.0.0/16"
    service_cidr        = "10.0.0.0/16"
    dns_service_ip      = "10.0.0.10"
  }
}

resource "azurerm_kubernetes_cluster_node_pool" "sandbox" {
  name                  = "sandbox"
  kubernetes_cluster_id = azurerm_kubernetes_cluster.this.id
  vm_size               = var.sandbox_vm_size
  os_sku                = "Ubuntu2404"
  os_disk_size_gb       = var.sandbox_os_disk_size_gb
  vnet_subnet_id        = azurerm_subnet.nodes.id
  node_count            = var.sandbox_node_count
  node_labels           = { "e2b.dev/role" = "sandbox" }
  node_taints           = ["e2b.dev/role=sandbox:NoSchedule"]
  tags                  = var.tags
  upgrade_settings {
    max_surge = "1"
  }
}

locals {
  kubelet_identity = azurerm_kubernetes_cluster.this.kubelet_identity[0]
}

resource "azurerm_role_assignment" "cluster_network" {
  scope                = azurerm_virtual_network.this.id
  role_definition_name = "Network Contributor"
  principal_id         = azurerm_kubernetes_cluster.this.identity[0].principal_id
}
