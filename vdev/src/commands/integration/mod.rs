crate::cli_subcommands! {
    r"Manage integration test environments...

These test setups are organized into a set of integrations, located in subdirectories
`scripts/integration`.  For each integration, there is a matrix of environments, described in the
`matrix` setting in the `test.yaml` file contained therein."

    mod show,
    mod build,
    mod start,
    mod stop,
    mod test,
    mod ci_paths,
}
