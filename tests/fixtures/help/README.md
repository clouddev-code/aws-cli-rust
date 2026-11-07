Output of the reference AWS CLI, captured with

    AWS_PAGER= aws <command...> help | col -b | expand

awsr does not reproduce one docutils quirk: a space before the period that
follows a link (`User Guide .`). The tests remove it from these files.
