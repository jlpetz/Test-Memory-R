TMR's configs

Put your own here, TM5 .cfg files or TMR .json configs, and run one by its name:

    tmr config=my_tm5.cfg

A config= that isn't in the folder you run tmr from is looked for here. TMR needs no config:
tmr plan=stability starts its built-in stability plan (plan= picks another; tmr --help lists
them). A plain tmr displays help and runs nothing. Double-clicking opens the window, where
Start begins the run.

demo_comprehensive_test.json and demo_tm5_compatible.json are TMR's examples, which
tmr --create-demo-configs writes again. An upgrade never overwrites a file here, and an uninstall
leaves this folder.
