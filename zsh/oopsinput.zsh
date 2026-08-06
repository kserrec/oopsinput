# oopsinput zsh plugin — capture at accept, shadow passthrough (M1).
#
# Contract with the binary (see src/main.rs header):
#   exit 0  -> run the original buffer unchanged
#   exit 10 -> replace buffer from fd 3 and run          (M2)
#   exit 11 -> keep buffer in ZLE for editing            (M3)
#   exit 12 -> cancel: clear buffer, run nothing         (M3)
#   anything else (timeout, crash, missing) -> fail open: run original
#
# Invariants (SPEC §9): the original buffer bytes are never altered on the
# allow/fail-open paths; the buffer travels over stdin, never argv.

# Interactive shells with ZLE only.
[[ -o interactive ]] || return 0

typeset -g _OOPSINPUT_BIN=${OOPSINPUT_BIN:-$HOME/.local/bin/oopsinput}
if [[ ! -x $_OOPSINPUT_BIN ]]; then
    # (V) renders control characters visibly (^[ etc.) — a hostile env value
    # cannot emit raw terminal sequences through this diagnostic. Note (qqqq)
    # is NOT sufficient: it wraps in $'...' but leaves control bytes raw.
    print -u2 "oopsinput: binary not found at ${(V)_OOPSINPUT_BIN} — guard disabled for this session"
    return 0
fi

# history/aliases/functions introspection (recency + typo candidate pool).
zmodload zsh/parameter 2>/dev/null

# Every ZLE widget that submits the buffer for execution.
typeset -ga _OOPSINPUT_WIDGETS
_OOPSINPUT_WIDGETS=(
    accept-line
    accept-line-and-down-history
    accept-and-hold
    accept-and-infer-next-history
)

# Invoke whatever this widget was before we wrapped it: a saved user widget
# (another plugin's wrapper) if one existed, else the ZLE builtin (.name).
_oopsinput_delegate() {
    local w=$1
    if (( ${+widgets[_oopsinput_orig_$w]} )); then
        zle _oopsinput_orig_$w
    else
        zle .$w
    fi
}

_oopsinput_handle() {
    local w=$1

    # Passthrough untouched: recursion, empty/whitespace buffer, PS2
    # continuation lines (only the initial line of a command is analyzed).
    if [[ -n $_OOPSINPUT_ACTIVE || -z ${BUFFER//[[:space:]]/} || ${CONTEXT:-start} != start ]]; then
        _oopsinput_delegate $w
        return $?
    fi
    local _OOPSINPUT_ACTIVE=1

    local original=$BUFFER

    # Resolution kind of the command word — only the live shell knows aliases
    # and functions. Enforced closed vocabulary: the raw word must never reach
    # argv (argv is world-readable), so anything unexpected becomes "unknown".
    # NOTE: explicit array assignment — ${${(z)BUFFER}[1]} string-indexes (first
    # *character*) when the split yields a single word. Regression-tested.
    local -a _oi_words
    _oi_words=( ${(z)BUFFER} )
    local word=${_oi_words[1]:-}
    local out kind=unknown
    if [[ -n $word ]]; then
        out=$(builtin whence -w -- "$word" 2>/dev/null)
        kind=${out##*: }
        case $kind in
            alias|function|builtin|command|hashed|reserved|none) ;;
            *) kind=unknown ;;
        esac
    fi

    # Recency relation (SPEC §5-L3): structural summaries of the last few
    # commands, computed HERE so no raw history text ever crosses to the
    # binary — per entry only: age, a shares-a-word bit (does the entry share
    # a non-command word with the current buffer, e.g. a target path typed
    # moments ago), and the first two words sanitized to [A-Za-z0-9_-]{1,32};
    # anything else — quoted words, URLs, values — collapses to "_". A secret
    # cannot survive that shape.
    #
    # History access is by direct event number: inside the widget HISTCMD is
    # the number the current line will get, so HISTCMD-age is the entry `age`
    # commands back. Never ${(Onk)history} — sorting every history key on
    # every accepted command cost ~8 ms at 10k entries, most of the whole
    # p50 latency budget (bughunt 2026-08-06, measured).
    #
    # Shared-word detection ignores flag words on both sides: a shared "-f"
    # is not "you referenced this target moments ago" (bughunt 2026-08-06).
    local -a _oi_recency=() _oi_bufrest=( ${(M)_oi_words[2,-1]:#[^-]*} ) _oi_hw
    local _oi_h _oi_w _oi_c1 _oi_c2
    integer _oi_age _oi_share
    for _oi_age in 1 2 3 4 5; do
        _oi_h=${history[$(( ${HISTCMD:-0} - _oi_age ))]:-}
        [[ -z $_oi_h ]] && continue
        _oi_hw=( ${(z)_oi_h} )
        _oi_share=0
        # (ie), not (Ie): forward exact search returns len+1 when absent;
        # the REVERSE form returns 0, which made this test always true —
        # shares was stuck at 1 (bughunt 2026-08-06, caught by the shim
        # payload dump: `1 1 ls -f` after zero shared words).
        for _oi_w in ${(M)_oi_hw[2,-1]:#[^-]*}; do
            if (( ${_oi_bufrest[(ie)$_oi_w]} <= ${#_oi_bufrest} )); then
                _oi_share=1; break
            fi
        done
        _oi_c1=${_oi_hw[1]:-}
        _oi_c2=${_oi_hw[2]:-}
        [[ -n $_oi_c1 && -z ${_oi_c1//[A-Za-z0-9_-]/} && ${#_oi_c1} -le 32 ]] || _oi_c1=_
        [[ -n $_oi_c2 && -z ${_oi_c2//[A-Za-z0-9_-]/} && ${#_oi_c2} -le 32 ]] || _oi_c2=_
        _oi_recency+=( "$_oi_age $_oi_share $_oi_c1 $_oi_c2" )
    done

    # Payload sections, NUL-separated (zsh strings can never contain NUL, so
    # the separator is collision-free): buffer, typo candidate pool, recency.
    # The candidate pool (every name only the live shell can see: aliases,
    # functions, builtins, reserved words) is sent only when the command word
    # resolves to nothing — L1 typo territory — so only that already-failing
    # path pays its cost.
    #
    # fd 3 is the replacement channel (SPEC §6): the binary's fd 3 is routed
    # into $captured; its stdout (decision JSON) and stderr are discarded.
    # Prompts reach the terminal via /dev/tty, not through these streams.
    local captured rc
    captured=$( {
        print -rn -- "$original"
        print -rn -- $'\0'
        if [[ $kind == none ]]; then
            print -rl -- ${(k)aliases} ${(k)functions} ${(k)builtins} ${(k)reswords}
        fi
        print -rn -- $'\0'
        (( ${#_oi_recency} )) && print -rl -- $_oi_recency
    } | "$_OOPSINPUT_BIN" check --res "$kind" 3>&1 >/dev/null 2>&1 )
    rc=$?

    case $rc in
        10)
            # replace: run the corrected buffer the user consented to.
            # SECURITY (SPEC §9): only with the integrity sentinel intact —
            # the binary terminates the exact replacement bytes with one NUL
            # (which also survives $(...)'s trailing-newline stripping). A
            # missing sentinel means a truncated or absent write: fail open,
            # run the original bytes unchanged.
            if [[ $captured == *$'\0' ]]; then
                BUFFER=${captured%$'\0'}
            else
                BUFFER=$original
            fi
            _oopsinput_delegate $w
            return $?
            ;;
        12)
            # cancel: run nothing
            BUFFER=""
            zle .reset-prompt 2>/dev/null
            return 0
            ;;
        11)
            # edit: leave the original buffer in ZLE
            BUFFER=$original
            zle .reset-prompt 2>/dev/null
            return 0
            ;;
        *)
            # 0 = allow; anything unexpected = fail open.
            # Either way the original bytes run unchanged.
            BUFFER=$original
            _oopsinput_delegate $w
            return $?
            ;;
    esac
}

# Wrap each accept widget, preserving any existing user widget.
() {
    local w
    for w in $_OOPSINPUT_WIDGETS; do
        (( ${+widgets[$w]} )) || continue
        case $widgets[$w] in
            user:_oopsinput_wrap_*) continue ;;              # already wrapped
            user:*) zle -A $w _oopsinput_orig_$w ;;          # save prior wrapper
        esac
        functions[_oopsinput_wrap_$w]="_oopsinput_handle $w"
        zle -N $w _oopsinput_wrap_$w
    done
}
