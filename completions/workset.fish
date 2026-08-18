# Fish completions for workset.
#
# Copy this file into ~/.config/fish/completions/ (or a directory on
# $fish_complete_path) and fish will autoload it.

# The binary expects COMP_LINE to hold the command line truncated at the
# cursor; fish filters the returned candidates by the current token itself.
complete --command workset --no-files \
    --arguments '(env _ARGCOMPLETE_=fish COMP_LINE=(commandline --cut-at-cursor --current-process) workset)'
