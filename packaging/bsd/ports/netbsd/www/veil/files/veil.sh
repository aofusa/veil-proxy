#!@RCD_SCRIPTS_SHELL@
#
# PROVIDE: veil
# REQUIRE: NETWORKING SERVERS
# KEYWORD: shutdown

$_rc_subr_loaded . /etc/rc.subr

name="veil"
rcvar=$name
command="@PREFIX@/bin/veil"
pidfile="@VARBASE@/run/${name}.pid"
required_files="@PKG_SYSCONFDIR@/config.toml"
command_args="& echo \$! > ${pidfile}"
extra_commands="reload configtest"
configtest_cmd="${command} -t"
start_precmd="${command} -t"

load_rc_config $name
run_rc_command "$1"
