# Binding inspection v1

The source-build inspection writer produced this synthetic, credential-free
document from the seventeen-presentation current-format binding seed. It pins
the ephemeral document and the embedded `CompiledProviderConfiguration` serde
encoding alongside semantic tool presentations. It is not a session export or
an executable request; mismatched versions and shapes are refused.

Regenerate with the ignored `generate_live_inspection_v1_pin` producer, setting
`PLAK_INSPECTION_FIXTURE_OUTPUT` to an output file, then run
`live_inspection_v1_preserves_source_configuration_and_presentation_bytes`.
