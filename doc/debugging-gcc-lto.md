# How to debug GCC LTO

Run the command with `-v -save-temps` and then extract the `lto1` line from the output and run that under the debugger.
