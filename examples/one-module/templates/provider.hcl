provider "mock" {
  region = "{{tfgen[stack.env].region}}"
}

terraform {
  required_providers {
    {{stack.__required_providers__}}
  }
}
