Output of the reference AWS CLI, as

    aws <command...> help | col -b | expand

shows it when the page is rendered with mandoc. Regenerate with
`generate.py` (see the docstring); the file names list the commands.

awsr does not reproduce one docutils quirk: a space before the period that
follows a link (`User Guide .`). The tests remove it from these files.
