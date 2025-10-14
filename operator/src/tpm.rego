package policy
import rego.v1
default hardware := 97
default configuration := 36
default executables := 33

tpm_pcrs_valid if {
  input.tpm.pcr04 in data.reference.tpm_pcr4
  input.tpm.pcr07 in data.reference.tpm_pcr7
  input.tpm.pcr14 in data.reference.tpm_pcr14
}

hardware := 2 if tpm_pcrs_valid
executables := 3 if tpm_pcrs_valid
configuration := 2 if tpm_pcrs_valid

default file_system := 0
default instance_identity := 0
default runtime_opaque := 0
default storage_opaque := 0
default sourced_data := 0
result := {
  "executables": executables,
  "hardware": hardware,
  "configuration": configuration,
  "file-system": file_system,
  "instance-identity": instance_identity,
  "runtime-opaque": runtime_opaque,
  "storage-opaque": storage_opaque,
  "sourced-data": sourced_data,
}
