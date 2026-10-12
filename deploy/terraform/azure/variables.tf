variable "subscription_id" {
  description = "Azure subscription to deploy into."
  type        = string
}

variable "name" {
  description = "Name prefix for every resource."
  type        = string
  default     = "e2b-rs"
  validation {
    condition     = can(regex("^[a-z][-a-z0-9]{3,20}[a-z0-9]$", var.name))
    error_message = "name must be 5 to 22 lowercase letters, digits and hyphens, starting with a letter."
  }
}

variable "location" {
  description = "Azure region."
  type        = string
  default     = "westeurope"
}

variable "domain" {
  description = "Domain the SDK uses as E2B_DOMAIN: the API answers on api.<domain> and sandboxes on <port>-<sandbox>.<domain>. It must be inside dns_zone_name."
  type        = string
}

variable "dns_zone_name" {
  description = "Existing Azure DNS zone that holds domain. Terraform adds the wildcard record and cert-manager answers the DNS-01 challenge in it."
  type        = string
}

variable "dns_zone_resource_group" {
  description = "Resource group of the DNS zone."
  type        = string
}

variable "acme_email" {
  description = "Email for the Let's Encrypt account."
  type        = string
}

variable "kubernetes_version" {
  description = "AKS version. Null takes the region's default."
  type        = string
  default     = null
}

variable "control_vm_size" {
  description = "VM size of the control pool: API, client proxy, stores, Envoy."
  type        = string
  default     = "Standard_D4ds_v5"
}

variable "control_min_count" {
  description = "Smallest number of control nodes; the cluster autoscaler adds more as the API and the proxy scale."
  type        = number
  default     = 2
}

variable "control_max_count" {
  description = "Largest number of control nodes."
  type        = number
  default     = 6
}

variable "sandbox_vm_size" {
  description = "VM size of the sandbox pool. It must offer nested virtualization (/dev/kvm): Intel Dsv3 and newer or AMD Dasv5 and newer, not burstable or arm64 sizes."
  type        = string
  default     = "Standard_D16ds_v5"
  validation {
    condition     = can(regex("^Standard_(D[0-9]+d?s_v[3-9]|D[0-9]+ad?s_v[5-9]|E[0-9]+d?s_v[3-9]|E[0-9]+ad?s_v[5-9]|L[0-9]+s_v[3-9])$", var.sandbox_vm_size))
    error_message = "sandbox_vm_size must be an x86-64 D, E or L size with nested virtualization, such as Standard_D16ds_v5."
  }
}

variable "sandbox_node_count" {
  description = "Number of sandbox nodes. Every node runs one orchestrator. Change it and apply again to scale; a removed node drains its sandboxes first."
  type        = number
  default     = 2
}

variable "sandbox_os_disk_size_gb" {
  description = "OS disk of a sandbox node; it holds the chunk cache and the sandbox files."
  type        = number
  default     = 256
}

variable "hugepages" {
  description = "2 MiB huge pages reserved on each sandbox node for guest memory. 8192 is 16 GiB; keep it at about half of the node's RAM."
  type        = number
  default     = 8192
}

variable "orchestrator_image" {
  description = "Orchestrator image to run. Empty builds docker/orchestrator.Dockerfile in the registry Terraform creates."
  type        = string
  default     = ""
}

variable "orchestrator_version" {
  description = "Tag of the image Terraform builds. Change it to build and roll out a new version."
  type        = string
  default     = "dev"
}

variable "tags" {
  description = "Tags on every resource."
  type        = map(string)
  default     = {}
}
