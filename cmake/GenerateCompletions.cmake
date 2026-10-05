# Generate btrdasd's bash, zsh and fish completions from a built binary.
#
# Run at BUILD time by the btrdasd_completions target in CMakeLists.txt:
#
#   cmake -DBTRDASD=<built btrdasd> -DOUT_DIR=<dir> -P cmake/GenerateCompletions.cmake
#
# Writes <OUT_DIR>/btrdasd.bash, <OUT_DIR>/_btrdasd and <OUT_DIR>/btrdasd.fish,
# which CMakeLists.txt then installs with install(FILES), so a DESTDIR install
# carries them like any other file (bd DAS-Backup-Manager-cway).
#
# Every failure stops the build and says why: the binary missing, exiting
# nonzero, killed, or printing nothing. Each file is written beside its final
# name and renamed into place only once it is whole and non-empty, and a
# failure removes all three, including any an earlier build left: a completion
# file is what this binary printed, whole, or absent — never truncated, never
# an older binary's, so an install after a failed build fails loudly.

# The file name each shell's completion loader looks for.
set(_file_bash "btrdasd.bash")
set(_file_zsh "_btrdasd")
set(_file_fish "btrdasd.fish")

function(_das_completions_fail why)
    foreach(_shell IN ITEMS bash zsh fish)
        file(REMOVE "${OUT_DIR}/${_file_${_shell}}" "${OUT_DIR}/${_file_${_shell}}.tmp")
    endforeach()
    message(FATAL_ERROR "${why}")
endfunction()

foreach(_var IN ITEMS BTRDASD OUT_DIR)
    if(NOT DEFINED ${_var} OR "${${_var}}" STREQUAL "")
        message(FATAL_ERROR "GenerateCompletions.cmake needs -D${_var}=...")
    endif()
endforeach()

if(NOT EXISTS "${BTRDASD}")
    _das_completions_fail("Cannot generate shell completions: ${BTRDASD} does not exist \
(the btrdasd_rust target, which this one depends on, builds it)")
endif()

file(MAKE_DIRECTORY "${OUT_DIR}")

foreach(_shell IN ITEMS bash zsh fish)
    set(_out "${OUT_DIR}/${_file_${_shell}}")
    set(_tmp "${_out}.tmp")
    execute_process(
        COMMAND "${BTRDASD}" completions ${_shell}
        OUTPUT_FILE "${_tmp}"
        ERROR_VARIABLE _err
        RESULT_VARIABLE _rc
    )
    # _rc is the exit status, or a message ("No such file or directory", a
    # signal name) when the command could not run or did not exit: anything
    # but 0 is a failure.
    if(NOT "${_rc}" STREQUAL "0")
        _das_completions_fail("'${BTRDASD} completions ${_shell}' failed (${_rc}): ${_err}")
    endif()
    file(SIZE "${_tmp}" _size)
    if(_size EQUAL 0)
        _das_completions_fail("'${BTRDASD} completions ${_shell}' exited 0 but printed nothing")
    endif()
    file(RENAME "${_tmp}" "${_out}")
endforeach()
