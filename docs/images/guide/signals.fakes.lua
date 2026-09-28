-- Show the two bindings after the button writes one new minute.
function __after()
    state("minute", 45):set(46)
end
