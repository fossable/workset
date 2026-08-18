# Bash completions for workset.
#
# Source this file from ~/.bashrc, or copy it into a bash-completion
# directory such as /usr/share/bash-completion/completions/.

# COMP_LINE and COMP_POINT are unexported shell variables during programmable
# completion, so they must be passed to the binary explicitly.
_workset_complete() {
    local IFS=$'\n'
    COMPREPLY=($(
        _ARGCOMPLETE_=bash \
        COMP_LINE="$COMP_LINE" \
        COMP_POINT="$COMP_POINT" \
        COMP_TYPE="$COMP_TYPE" \
        COMP_KEY="$COMP_KEY" \
        "$1" 2>/dev/null
    ))
}

complete -o bashdefault -o default -F _workset_complete workset
